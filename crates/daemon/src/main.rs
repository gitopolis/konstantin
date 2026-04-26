//! `screentimed` — privileged daemon for enforcing daily screen-time limits.
//!
//! v1 scope (this commit):
//!   * load TOML config
//!   * bind a Unix socket and accept connections
//!   * authenticate clients via SO_PEERCRED (LOCAL_PEERCRED on macOS)
//!   * answer `GetStatus` with stub used-seconds (always 0 in this scaffold)
//!
//! Out of scope until later phases:
//!   * session enumeration via utmpx
//!   * counter persistence and midnight reset
//!   * forced logout via `launchctl bootout`
//!   * the `Subscribe` push channel (returns Error for now)

mod config;
mod ipc;

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();

    let config_path = std::env::var("SCREENTIMED_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/screentimed/config.toml"));

    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("loading config at {}", config_path.display()))?;
    info!(
        socket = %cfg.socket_path.display(),
        users = cfg.users.len(),
        "config loaded"
    );

    let server = ipc::Server::bind(cfg.clone())
        .await
        .context("binding IPC socket")?;

    // Graceful shutdown on SIGTERM / SIGINT.
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("received SIGINT, shutting down"),
            _ = term.recv() => info!("received SIGTERM, shutting down"),
        }
    };

    tokio::select! {
        result = server.run() => {
            if let Err(e) = result {
                warn!(error = %e, "server task exited with error");
            }
        }
        _ = shutdown => {}
    }

    Ok(())
}

fn install_tracing() {
    let filter = EnvFilter::try_from_env("SCREENTIMED_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,screentimed=debug,screentime_proto=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();
}
