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
use crate::uninstall;
use anyhow::{Context, Result};
use nix::unistd::{getpeereid, User};
use konstantin_proto::{
    read_frame, write_frame, AutostartEntry, FrameError, Request, Response, SessionState,
    UserStatus,
};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
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
    /// Path the IPC `WriteConfig` handler atomically rewrites.
    /// Threaded through from `main` so callers can override (tests,
    /// dev-tree). Read by `ReadConfig` too.
    config_path: PathBuf,
}

impl Server {
    pub async fn bind(
        cfg: Config,
        config_path: PathBuf,
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
            config_path,
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
            let config_path = self.config_path.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(stream, cfg, config_path, state, tick_tx).await
                {
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
    config_path: PathBuf,
    state: Arc<Mutex<State>>,
    tick_tx: broadcast::Sender<()>,
) -> Result<()> {
    let (uid, gid) = peer_creds(&stream).context("reading peer credentials")?;
    let username = User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("uid:{uid}"));
    // Cache admin-membership per-connection. Group memberships
    // change rarely; re-checking on every frame is wasteful, and
    // re-checking might give an inconsistent answer mid-flight.
    let admin = peer_is_admin(uid);
    debug!(uid, gid, %username, admin, "client connected");

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
            Request::ReadConfig => {
                let resp = match std::fs::read_to_string(&config_path) {
                    Ok(contents) => Response::Config { contents },
                    Err(e) => Response::Error {
                        message: format!("read config: {e}"),
                    },
                };
                write_frame(&mut writer, &resp).await?;
            }
            Request::WriteConfig { contents } => {
                if !admin {
                    write_frame(&mut writer, &perm_denied()).await?;
                    continue;
                }
                let resp = handle_write_config(&config_path, &contents);
                write_frame(&mut writer, &resp).await?;
                if matches!(resp, Response::Ack) {
                    // Schedule a clean exit after the response has been
                    // flushed. KeepAlive=true (or SMAppService) brings
                    // us back up immediately with the new config.
                    tokio::spawn(async {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        info!("WriteConfig succeeded; exiting for respawn-on-reload");
                        std::process::exit(0);
                    });
                }
            }
            Request::ReadAutostartManifest => {
                let resp = Response::AutostartManifest {
                    entries: read_autostart_manifest(),
                };
                write_frame(&mut writer, &resp).await?;
            }
            Request::WriteAutostart { username: u, enable } => {
                if !admin {
                    write_frame(&mut writer, &perm_denied()).await?;
                    continue;
                }
                let resp = handle_write_autostart(&u, enable);
                write_frame(&mut writer, &resp).await?;
            }
            Request::SelfUninstall => {
                if !admin {
                    write_frame(&mut writer, &perm_denied()).await?;
                    continue;
                }
                // Ack before exiting — clients should not block on the
                // teardown completing. The blocking task takes over
                // after the Ack flush; KeepAlive may respawn us, and
                // the tray's `sm::unregister_daemon()` finishes the
                // job by killing the respawn and unregistering with
                // SMAppService.
                write_frame(&mut writer, &Response::Ack).await?;
                tokio::task::spawn_blocking(uninstall::self_uninstall);
                return Ok(());
            }
        }
    }
}

/// Validate, atomically write, and report success on the config file.
/// Validation parses through the live `Config` schema so we never
/// commit bytes that would crash the daemon at restart.
fn handle_write_config(path: &Path, contents: &str) -> Response {
    if let Err(e) = Config::parse(contents) {
        return Response::Error {
            message: format!("invalid config: {e}"),
        };
    }
    if let Err(e) = atomic_write_0600(path, contents.as_bytes()) {
        return Response::Error {
            message: format!("write config: {e}"),
        };
    }
    info!(path = %path.display(), "config rewritten via IPC");
    Response::Ack
}

/// Mode-0600 atomic write: write to `<path>.tmp`, fsync, rename.
/// Same shape as `state.json`'s persist. Owner stays root.
fn atomic_write_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("opening {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Probe each `/Users/<name>/Library/LaunchAgents/com.gitopolis.
/// konstantin-tray.plist` and report whether it exists. `nobody`,
/// `_system`, `Shared`, `Guest`, hidden directories are skipped.
fn read_autostart_manifest() -> Vec<AutostartEntry> {
    const PLIST_REL: &str =
        "Library/LaunchAgents/com.gitopolis.konstantin-tray.plist";
    let mut entries = Vec::new();
    let Ok(it) = std::fs::read_dir("/Users") else {
        return entries;
    };
    for ent in it.flatten() {
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('_')
            || name.starts_with('.')
            || matches!(name, "Shared" | "Guest" | "nobody" | "daemon" | "root")
        {
            continue;
        }
        // Real-user check via getpwnam.
        if !matches!(
            User::from_name(name),
            Ok(Some(_))
        ) {
            continue;
        }
        let plist = path.join(PLIST_REL);
        entries.push(AutostartEntry {
            username: name.to_string(),
            enabled: plist.exists(),
        });
    }
    entries.sort_by(|a, b| a.username.cmp(&b.username));
    entries
}

/// Install or remove the per-user tray autostart plist. The plist's
/// `Program` value is derived from the live tray binary path embedded
/// in the operator's existing plist if present (so we don't have to
/// hard-code a bundle location); otherwise we fall back to the
/// canonical `/Applications/Konstantin.app` path.
fn handle_write_autostart(username: &str, enable: bool) -> Response {
    let user = match User::from_name(username) {
        Ok(Some(u)) => u,
        _ => {
            return Response::Error {
                message: format!("unknown user '{username}'"),
            };
        }
    };
    let plist_dir = user.dir.join("Library/LaunchAgents");
    let plist = plist_dir.join("com.gitopolis.konstantin-tray.plist");
    if !enable {
        // Bootout best-effort, then remove the plist file.
        let _ = std::process::Command::new("/bin/launchctl")
            .args([
                "bootout",
                &format!("gui/{}/com.gitopolis.konstantin-tray", user.uid),
            ])
            .status();
        match std::fs::remove_file(&plist) {
            Ok(()) | Err(_) => {}
        }
        return Response::Ack;
    }
    if let Err(e) = std::fs::create_dir_all(&plist_dir) {
        return Response::Error {
            message: format!("creating {}: {e}", plist_dir.display()),
        };
    }
    let exe = autostart_tray_exe();
    let body = build_user_launchagent_plist(&exe);
    if let Err(e) = std::fs::write(&plist, body) {
        return Response::Error {
            message: format!("writing {}: {e}", plist.display()),
        };
    }
    // chown to the target user so they can read/edit it themselves.
    use std::os::unix::ffi::OsStrExt;
    let path_c = match CString::new(plist.as_os_str().as_bytes()) {
        Ok(s) => s,
        Err(_) => {
            return Response::Error {
                message: "plist path contained NUL".into(),
            };
        }
    };
    // SAFETY: chown of an existing path; uid/gid are valid u32s.
    unsafe {
        libc::chown(path_c.as_ptr(), user.uid.as_raw(), user.gid.as_raw());
    }
    // Best-effort GUI-domain bootstrap. Ignored if the user isn't
    // logged in; a future login picks the plist up automatically.
    let _ = std::process::Command::new("/bin/launchctl")
        .args([
            "bootstrap",
            &format!("gui/{}", user.uid),
            &plist.display().to_string(),
        ])
        .status();
    Response::Ack
}

/// Resolve the tray binary path the daemon should bake into freshly
/// installed user-autostart plists. Mirrors the tray's own
/// `install::ensure_user_launchagent` policy: prefer the bundle's
/// `Contents/MacOS/konstantin-tray`. Falls back to the canonical
/// `/Applications/Konstantin.app` path if the daemon's bundle marker
/// is unset (dev-tree daemon was started by hand).
fn autostart_tray_exe() -> PathBuf {
    let marker = Path::new(uninstall::MARKER_PATH);
    if let Ok(s) = std::fs::read_to_string(marker) {
        let bundle = PathBuf::from(s.trim());
        let candidate = bundle.join("Contents/MacOS/konstantin-tray");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("/Applications/Konstantin.app/Contents/MacOS/konstantin-tray")
}

fn build_user_launchagent_plist(tray_exe: &Path) -> String {
    let exe = xml_escape(&tray_exe.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.gitopolis.konstantin-tray</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>StandardOutPath</key>
    <string>/tmp/konstantin-tray.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/konstantin-tray.err.log</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn perm_denied() -> Response {
    Response::Error {
        message: "permission denied (admin group required)".into(),
    }
}

/// True iff `uid` is a member of the macOS `admin` group (gid 80).
/// macOS doesn't expose `getgrouplist` on Apple targets via `nix`,
/// so we walk the group's `gr_mem` membership list ourselves and
/// also accept the user whose primary gid is 80.
fn peer_is_admin(uid: u32) -> bool {
    let user = match User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(u)) => u,
        _ => return false,
    };
    if user.gid.as_raw() == ADMIN_GID {
        return true;
    }
    let members = read_admin_group_members();
    is_admin_user(&members, &user.name)
}

const ADMIN_GID: u32 = 80;

/// Pure half of [`peer_is_admin`]. Easy to unit-test against a fake
/// member list.
fn is_admin_user(member_names: &[String], username: &str) -> bool {
    member_names.iter().any(|m| m == username)
}

/// Snapshot the `gr_mem` of the `admin` group via `getgrnam(3)`. Best-
/// effort: returns an empty Vec if Open Directory is unreachable.
fn read_admin_group_members() -> Vec<String> {
    let cname = match CString::new("admin") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    // SAFETY: getgrnam returns a pointer to libc-managed static
    // storage; we read it before any other libc call that could
    // overwrite it. The `gr_mem` array is NULL-terminated.
    unsafe {
        let g = libc::getgrnam(cname.as_ptr());
        if g.is_null() {
            return out;
        }
        let mut p = (*g).gr_mem;
        if p.is_null() {
            return out;
        }
        while !(*p).is_null() {
            if let Ok(s) = CStr::from_ptr(*p).to_str() {
                out.push(s.to_string());
            }
            p = p.add(1);
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::is_admin_user;

    #[test]
    fn admin_membership_pure() {
        let members: Vec<String> = ["root", "alice"].iter().map(|s| s.to_string()).collect();
        assert!(is_admin_user(&members, "alice"));
        assert!(is_admin_user(&members, "root"));
        assert!(!is_admin_user(&members, "bob"));
        assert!(!is_admin_user(&[], "alice"));
    }
}
