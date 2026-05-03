//! Shared client logic for talking to `screentimed`. The headless
//! `konstantin-status` binary and the menu-bar `konstantin-tray` binary
//! both use this.

pub mod notifications;
pub mod users;

use anyhow::{Context, Result};
use konstantin_proto::{
    read_frame, write_frame, AutostartEntry, FrameError, Request, Response, UserStatus,
    DEFAULT_SOCKET_PATH,
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

/// One-shot IPC call: open the socket, send `req`, read a single
/// response, close. Errors map `Response::Error` to `anyhow::Error`
/// so callers don't have to match on the variant.
pub async fn one_shot(socket: &Path, req: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    write_frame(&mut stream, &req).await.context("sending request")?;
    let resp: Response = read_frame(&mut stream).await.context("reading response")?;
    if let Response::Error { message } = &resp {
        anyhow::bail!("daemon error: {message}");
    }
    Ok(resp)
}

/// Sync wrapper: spins a single-thread tokio runtime, runs `req`, and
/// returns the response. Intended for use from AppKit's main-thread
/// flows (like Configure save) where calls are short and don't warrant
/// running an async runtime alongside the run loop.
pub fn one_shot_sync(req: Request) -> Result<Response> {
    let socket = default_socket_path();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("building tokio runtime for IPC call")?;
    rt.block_on(one_shot(&socket, req))
}

/// Read the daemon's active config file as TOML text. Convenience
/// wrapper over `one_shot_sync(Request::ReadConfig)` that unwraps the
/// `Response::Config` payload.
pub fn read_config_sync() -> Result<String> {
    match one_shot_sync(Request::ReadConfig)? {
        Response::Config { contents } => Ok(contents),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Read the per-user tray-autostart plist presence manifest.
pub fn read_autostart_manifest_sync() -> Result<Vec<AutostartEntry>> {
    match one_shot_sync(Request::ReadAutostartManifest)? {
        Response::AutostartManifest { entries } => Ok(entries),
        other => anyhow::bail!("unexpected response: {other:?}"),
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
