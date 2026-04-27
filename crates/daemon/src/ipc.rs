//! Unix-socket IPC server.
//!
//! On `accept` we use `getpeereid(2)` to learn the caller's real UID. The
//! daemon never trusts UIDs sent over the wire — every response is scoped
//! to the peer UID. (Root callers may later be allowed cross-user queries;
//! not implemented yet.)
//!
//! Two request lifecycles:
//!
//! * One-shot (`GetStatus`, `ReportSessionState`) — read a frame, write a
//!   frame, loop.
//! * Long-lived (`Subscribe`) — write one immediate `StatusUpdate`, then
//!   push a fresh `StatusUpdate` every time the shared `tick_tx` broadcast
//!   fires (the ticker does this every `tick_seconds`; the midnight
//!   resetter does it on rollover). The connection ends on either side
//!   closing.

use crate::config::Config;
use crate::state::State;
use crate::time::next_local_midnight;
use anyhow::{Context, Result};
use nix::unistd::{getpeereid, User};
use screentime_proto::{
    read_frame, write_frame, FrameError, Request, Response, SessionState, UserStatus,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

pub struct Server {
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
    tick_tx: broadcast::Sender<()>,
    listener: UnixListener,
    socket_path: PathBuf,
}

impl Server {
    pub async fn bind(
        cfg: Config,
        state: Arc<Mutex<State>>,
        tick_tx: broadcast::Sender<()>,
    ) -> Result<Self> {
        let socket_path = cfg.socket_path.clone();

        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating socket dir {}", parent.display()))?;
            }
        }

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;

        // 0666 so any local user can connect; we authenticate via peer creds.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o666);
        std::fs::set_permissions(&socket_path, perms)
            .with_context(|| format!("chmod 666 {}", socket_path.display()))?;

        info!(path = %socket_path.display(), "listening");

        Ok(Self {
            cfg: Arc::new(cfg),
            state,
            tick_tx,
            listener,
            socket_path,
        })
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, _addr) = match self.listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let cfg = self.cfg.clone();
            let state = self.state.clone();
            let tick_tx = self.tick_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, cfg, state, tick_tx).await {
                    debug!(error = %e, "connection ended");
                }
            });
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_connection(
    stream: UnixStream,
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
    tick_tx: broadcast::Sender<()>,
) -> Result<()> {
    let (uid, gid) = peer_creds(&stream).context("reading peer credentials")?;
    let username = User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("uid:{uid}"));
    debug!(uid, gid, %username, "client connected");

    let (mut reader, mut writer) = stream.into_split();

    loop {
        let req: Request = match read_frame(&mut reader).await {
            Ok(r) => r,
            Err(FrameError::Closed) => return Ok(()),
            Err(e) => {
                let _ = write_frame(
                    &mut writer,
                    &Response::Error {
                        message: format!("bad frame: {e}"),
                    },
                )
                .await;
                return Err(e.into());
            }
        };

        match req {
            Request::GetStatus => {
                let resp = Response::Status(compute_status(&username, uid, &cfg, &state));
                write_frame(&mut writer, &resp).await?;
            }
            Request::ReportSessionState { .. } => {
                write_frame(&mut writer, &Response::Ack).await?;
            }
            Request::Subscribe => {
                debug!(uid, %username, "client subscribed");
                let rx = tick_tx.subscribe();
                run_subscribe(&mut reader, &mut writer, &username, uid, &cfg, &state, rx).await?;
                return Ok(());
            }
        }
    }
}

/// Long-lived push loop. Returns when the client closes, the broadcast
/// closes, or the write side errors.
async fn run_subscribe(
    reader: &mut OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
    username: &str,
    uid: u32,
    cfg: &Config,
    state: &Mutex<State>,
    mut rx: broadcast::Receiver<()>,
) -> Result<()> {
    // Per the proto doc, fire one immediate update so the client doesn't
    // have to wait a full tick to see its initial state.
    let initial = Response::StatusUpdate(compute_status(username, uid, cfg, state));
    write_frame(writer, &initial).await?;

    loop {
        tokio::select! {
            biased;
            wake = rx.recv() => match wake {
                Ok(()) => {
                    let resp = Response::StatusUpdate(compute_status(username, uid, cfg, state));
                    if let Err(e) = write_frame(writer, &resp).await {
                        debug!(error = %e, "subscribe: write failed, ending session");
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(uid, %username, skipped = n, "subscriber lagged; sending fresh snapshot");
                    let resp = Response::StatusUpdate(compute_status(username, uid, cfg, state));
                    if let Err(e) = write_frame(writer, &resp).await {
                        debug!(error = %e, "subscribe: write failed after lag, ending session");
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("subscribe: tick broadcast closed (daemon shutdown?)");
                    return Ok(());
                }
            },
            // Detect client close. We don't expect any further frames after
            // Subscribe, so an `Ok(_)` here is unexpected — log and ignore.
            next = read_frame::<_, Request>(reader) => match next {
                Err(FrameError::Closed) => return Ok(()),
                Err(e) => {
                    debug!(error = %e, "subscribe: client read error, ending session");
                    return Ok(());
                }
                Ok(_) => {
                    debug!(uid, %username, "subscribe: ignoring unexpected client frame");
                }
            }
        }
    }
}

/// Build a `UserStatus` for the calling peer from current counters + the
/// last tick's enumeration of console sessions.
fn compute_status(username: &str, uid: u32, cfg: &Config, state: &Mutex<State>) -> UserStatus {
    let (used, active) = {
        let s = state.lock().expect("state mutex poisoned");
        (s.used(username), s.is_active(username))
    };

    let (session_state, daily_limit_seconds) = match cfg.user_by_name(username) {
        None => (SessionState::NotConfigured, 0u32),
        Some(u) => {
            let limit = u.daily_limit_minutes.saturating_mul(60);
            let st = if used >= limit {
                SessionState::LimitReached
            } else if active {
                SessionState::Active
            } else {
                SessionState::Offline
            };
            (st, limit)
        }
    };

    UserStatus {
        uid,
        username: username.to_string(),
        state: session_state,
        daily_limit_seconds,
        used_seconds: used,
        remaining_seconds: daily_limit_seconds as i64 - used as i64,
        resets_at: next_local_midnight(),
        warn_thresholds_minutes: cfg.warn_thresholds_minutes.clone(),
    }
}

/// macOS / BSD: peer credentials via `getpeereid(2)`. Returns `(uid, gid)`.
fn peer_creds(stream: &UnixStream) -> Result<(u32, u32)> {
    let (uid, gid) = getpeereid(stream).context("getpeereid")?;
    Ok((uid.as_raw(), gid.as_raw()))
}
