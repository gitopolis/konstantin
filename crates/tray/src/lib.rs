//! Shared client logic for talking to `screentimed`. The headless `status`
//! binary and the (forthcoming) `tray` binary both use this.

use anyhow::{Context, Result};
use screentime_proto::{
    read_frame, write_frame, Request, Response, UserStatus, DEFAULT_SOCKET_PATH,
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
