//! Unix-socket IPC server.
//!
//! On `accept` we use `getpeereid(2)` (via `nix`) to learn the caller's real
//! UID. The daemon never trusts UIDs sent over the wire — every response is
//! scoped to the peer UID. (Root callers may later be allowed cross-user
//! queries; not implemented yet.)

use crate::config::Config;
use crate::state::State;
use crate::time::next_local_midnight;
use anyhow::{Context, Result};
use nix::unistd::User;
use screentime_proto::{
    read_frame, write_frame, Request, Response, SessionState, UserStatus,
};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

pub struct Server {
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
    listener: UnixListener,
    socket_path: PathBuf,
}

impl Server {
    pub async fn bind(cfg: Config, state: Arc<Mutex<State>>) -> Result<Self> {
        let socket_path = cfg.socket_path.clone();

        // Make sure the parent directory exists (e.g. /var/run on macOS does,
        // but /var/run/screentime/ if you ever nest).
        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating socket dir {}", parent.display()))?;
            }
        }

        // Stale socket from a previous run? Remove it.
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;

        // 0666 so any local user can connect; we authenticate via peer creds.
        // If you want to restrict to a specific group later, chown + 0660.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o666);
        std::fs::set_permissions(&socket_path, perms)
            .with_context(|| format!("chmod 666 {}", socket_path.display()))?;

        info!(path = %socket_path.display(), "listening");

        Ok(Self {
            cfg: Arc::new(cfg),
            state,
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
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, cfg, state).await {
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
    mut stream: UnixStream,
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
) -> Result<()> {
    let (uid, gid) = peer_creds(&stream).context("reading peer credentials")?;
    let username = User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("uid:{uid}"));
    debug!(uid, gid, %username, "client connected");

    loop {
        let req: Request = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(screentime_proto::FrameError::Closed) => return Ok(()),
            Err(e) => {
                let _ = write_frame(
                    &mut stream,
                    &Response::Error {
                        message: format!("bad frame: {e}"),
                    },
                )
                .await;
                return Err(e.into());
            }
        };

        let resp = handle_request(req, &username, uid, &cfg, &state);
        write_frame(&mut stream, &resp).await?;
    }
}

fn handle_request(
    req: Request,
    username: &str,
    uid: u32,
    cfg: &Config,
    state: &Mutex<State>,
) -> Response {
    match req {
        Request::GetStatus => Response::Status(compute_status(username, uid, cfg, state)),
        Request::Subscribe => Response::Error {
            message: "subscribe not implemented yet (v1 scaffold)".into(),
        },
        Request::ReportSessionState { .. } => Response::Ack,
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
    }
}

/// macOS / BSD: peer credentials via `getpeereid(2)`. Returns `(uid, gid)`.
///
/// `nix` 0.29 doesn't re-export `getpeereid`, so we call libc directly.
fn peer_creds(stream: &UnixStream) -> Result<(u32, u32)> {
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, open Unix-socket file descriptor borrowed from
    // `stream` for the duration of this call; the out-pointers point at local
    // stack slots we own.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid");
    }
    Ok((uid, gid))
}
