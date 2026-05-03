//! Wire types and framing for the screentime daemon / tray IPC.
//!
//! Frame format on the Unix socket:
//!
//! ```text
//! +-----------------+----------------------+
//! |  u32 length BE  |  JSON payload bytes  |
//! +-----------------+----------------------+
//! ```
//!
//! One frame per request and per response. The client opens a connection,
//! sends a `Request`, and reads one or more `Response` frames. For
//! `Request::Subscribe` the daemon keeps the connection open and pushes
//! `Response::StatusUpdate` frames whenever the user's status changes.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame payload size we'll accept. Generous; real frames are tiny.
pub const MAX_FRAME_BYTES: usize = 1 << 20; // 1 MiB

/// Default daemon socket path.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/screentimed.sock";

/// Requests sent from a client to the daemon.
///
/// **Auth model.** All requests are accepted from any peer connected
/// to the socket — peer credentials (uid/gid) are derived via
/// `getpeereid`. Daemon-side handlers gate sensitive ops on
/// `peer_is_admin(uid)` (uid is a member of the macOS `admin` group,
/// gid 80). The `admin: true` markers in this enum's doc comments
/// indicate where that gate fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// One-shot: return the caller's current status.
    GetStatus,
    /// Long-lived: server pushes `StatusUpdate` whenever the caller's status
    /// changes (and once immediately on connect).
    Subscribe,
    /// v2: client reports its own session lock / idle state. Daemon may use
    /// this to pause counting. Ignored in v1.
    ReportSessionState {
        locked: bool,
        idle_seconds: u32,
    },
    /// Read the active config TOML. Open to all peers — the file is
    /// sensitive (mode 0600) but reading via the daemon is the
    /// non-privileged way for a tray to display current settings.
    ReadConfig,
    /// Replace the on-disk config and reload it in-process. **admin**.
    /// `contents` must parse as the daemon's TOML schema; the daemon
    /// validates before writing and rejects malformed input.
    /// On success, all subscribers receive a refreshed `StatusUpdate`.
    WriteConfig { contents: String },
    /// Probe each local user's tray-autostart LaunchAgent plist and
    /// report which ones are present. Open to all peers.
    ReadAutostartManifest,
    /// Install or remove `<home>/Library/LaunchAgents/com.gitopolis.
    /// konstantin-tray.plist` for the named user. **admin**.
    WriteAutostart { username: String, enable: bool },
    /// Tear down: bootout the daemon, remove `/var/db/screentimed/`,
    /// remove every user's tray-autostart plist, and exit. **admin**.
    /// The daemon ACKs as soon as it has committed to dying — clients
    /// may see EOF immediately afterwards.
    SelfUninstall,
}

/// Responses from the daemon to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Status(UserStatus),
    StatusUpdate(UserStatus),
    Ack,
    Error {
        message: String,
    },
    /// Successful response to `Request::ReadConfig`. `contents` is the
    /// raw TOML text the daemon read from disk — clients deserialize
    /// it themselves so the daemon doesn't have to know about every
    /// schema permutation.
    Config {
        contents: String,
    },
    /// Successful response to `Request::ReadAutostartManifest`.
    AutostartManifest {
        entries: Vec<AutostartEntry>,
    },
}

/// One row of the `ReadAutostartManifest` response: a local user and
/// whether their tray-autostart plist is currently installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutostartEntry {
    pub username: String,
    pub enabled: bool,
}

/// What the daemon thinks is going on for a particular user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatus {
    pub uid: u32,
    pub username: String,
    pub state: SessionState,
    pub daily_limit_seconds: u32,
    pub used_seconds: u32,
    /// Signed so a value < 0 means "in grace period / over the limit".
    pub remaining_seconds: i64,
    pub resets_at: DateTime<Local>,
    /// Tray-side notification thresholds in minutes (e.g. `[15, 5, 1]`).
    /// Populated from the daemon's `warn_thresholds_minutes` config.
    /// `#[serde(default)]` so older daemons (without the field) still
    /// deserialize correctly — newer trays just see no thresholds and
    /// fire no notifications.
    #[serde(default)]
    pub warn_thresholds_minutes: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// User is logged in and the timer is advancing.
    Active,
    /// Logged in but timer paused (locked / idle / fast-user-switched away).
    Paused,
    /// Daily limit reached; pending logout.
    LimitReached,
    /// User is configured but not currently logged in.
    Offline,
    /// Username not present in the daemon config — unrestricted.
    NotConfigured,
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame too large ({size} bytes, max {max})")]
    FrameTooLarge { size: usize, max: usize },
    #[error("connection closed by peer")]
    Closed,
}

/// Read one length-prefixed JSON frame and deserialize it into `T`.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Closed);
        }
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let value = serde_json::from_slice::<T>(&buf)?;
    Ok(value)
}

/// Serialize `value` and write it as a single length-prefixed JSON frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            size: payload.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_status_response() {
        let (mut a, mut b) = duplex(4096);
        let resp = Response::Status(UserStatus {
            uid: 501,
            username: "alice".into(),
            state: SessionState::Active,
            daily_limit_seconds: 7200,
            used_seconds: 60,
            remaining_seconds: 7140,
            resets_at: Local::now(),
            warn_thresholds_minutes: vec![15, 5, 1],
        });
        write_frame(&mut a, &resp).await.unwrap();
        let got: Response = read_frame(&mut b).await.unwrap();
        match got {
            Response::Status(s) => assert_eq!(s.username, "alice"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn roundtrip_request() {
        let (mut a, mut b) = duplex(4096);
        write_frame(&mut a, &Request::GetStatus).await.unwrap();
        let got: Request = read_frame(&mut b).await.unwrap();
        assert!(matches!(got, Request::GetStatus));
    }
}
