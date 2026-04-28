//! Shared client logic for talking to `screentimed`. The headless
//! `konstantin-status` binary and the menu-bar `konstantin-tray` binary
//! both use this.

pub mod notifications;
pub mod users;

use anyhow::{Context, Result};
use konstantin_proto::{
    read_frame, write_frame, FrameError, Request, Response, UserStatus, DEFAULT_SOCKET_PATH,
};
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

/// Resolve the daemon socket path: `$SCREENTIMED_SOCKET` if set, else
/// the compile-time default.
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("SCREENTIMED_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

/// Open a connection, send `GetStatus`, read one response, return it.
pub async fn fetch_status(socket: &Path) -> Result<UserStatus> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    write_frame(&mut stream, &Request::GetStatus)
        .await
        .context("sending GetStatus")?;
    let resp: Response = read_frame(&mut stream).await.context("reading response")?;
    match resp {
        Response::Status(s) => Ok(s),
        Response::Error { message } => anyhow::bail!("daemon error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// A live subscription to the daemon's `StatusUpdate` push stream.
///
/// Built by [`Subscription::open`], drained by repeatedly calling
/// [`Subscription::next_update`]. The daemon pushes one frame on subscribe,
/// then one per tick (default every 5 s) and on midnight rollover. The
/// stream ends when the daemon closes the connection (returns
/// `Ok(None)`) or on a transport error.
pub struct Subscription {
    stream: UnixStream,
}

impl Subscription {
    /// Open a connection to `socket` and send `Request::Subscribe`. Does
    /// not wait for the first push — call `next_update` for that.
    pub async fn open(socket: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to {}", socket.display()))?;
        write_frame(&mut stream, &Request::Subscribe)
            .await
            .context("sending Subscribe")?;
        Ok(Self { stream })
    }

    /// Read the next pushed `UserStatus`. `Ok(None)` means the daemon
    /// closed the connection cleanly.
    pub async fn next_update(&mut self) -> Result<Option<UserStatus>> {
        let resp: Response = match read_frame(&mut self.stream).await {
            Ok(r) => r,
            Err(FrameError::Closed) => return Ok(None),
            Err(e) => return Err(e).context("reading subscribe frame"),
        };
        match resp {
            Response::StatusUpdate(s) => Ok(Some(s)),
            Response::Error { message } => anyhow::bail!("daemon error: {message}"),
            other => anyhow::bail!("unexpected response on subscribe stream: {other:?}"),
        }
    }
}

/// Format remaining seconds as `HhMm` / `MmSs`.
pub fn format_remaining(secs: i64) -> String {
    if secs < 0 {
        return format!("-{}", format_remaining(-secs));
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}
