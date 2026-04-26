//! `screentimed` — privileged daemon for enforcing daily screen-time limits.
//!
//! v1 scope (phase 2):
//!   * load TOML config
//!   * bind a Unix socket and accept connections
//!   * authenticate clients via `getpeereid(2)`
//!   * enumerate console sessions via `utmpx` and increment per-user
//!     counters every `tick_seconds`, persisting to `state.json`
//!   * answer `GetStatus` with real used / remaining seconds and a
//!     resolved `SessionState`
//!
//! Out of scope until later phases:
//!   * scheduled midnight reset task (phase 3 — for now the tick path
//!     resets opportunistically when it notices the date has changed)
//!   * forced logout via `launchctl bootout` (phase 5)
//!   * `Subscribe` push channel (phase 4 — returns Error for now)

mod config;
mod ipc;
mod sessions;
mod state;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
        state  = %cfg.state_path.display(),
        users  = cfg.users.len(),
        tick_s = cfg.tick_seconds,
        "config loaded"
    );

    let state = Arc::new(Mutex::new(state::State::load(&cfg.state_path)));

    // Roll over immediately if the on-disk state belongs to a previous day.
    {
        let today = chrono::Local::now().date_naive();
        let mut s = state.lock().expect("state mutex");
        if s.reset_if_new_day(today) {
            info!(%today, "counters reset for new day on startup");
        }
    }

    // Populate `active_now` once before the first tick fires so an
    // immediate `GetStatus` returns an accurate `Active` / `Offline`.
    {
        let initial = sessions::console_users();
        let mut s = state.lock().expect("state mutex");
        s.active_now = initial;
    }

    let server = ipc::Server::bind(cfg.clone(), state.clone())
        .await
        .context("binding IPC socket")?;

    let ticker_state = state.clone();
    let ticker_cfg = cfg.clone();
    tokio::spawn(async move {
        run_ticker(ticker_cfg, ticker_state).await;
    });

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

/// Periodically: enumerate console sessions, advance counters, persist.
async fn run_ticker(cfg: config::Config, state: Arc<Mutex<state::State>>) {
    let period = Duration::from_secs(cfg.tick_seconds as u64);
    let mut iv = tokio::time::interval(period);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick of `interval` completes immediately; consume it so the
    // first counter increment happens after a real `period` has elapsed.
    iv.tick().await;

    loop {
        iv.tick().await;
        let active = sessions::console_users();
        let today = chrono::Local::now().date_naive();

        let snapshot = {
            let mut s = state.lock().expect("state mutex");
            if s.reset_if_new_day(today) {
                info!(%today, "counters reset for new day");
            }
            s.tick(&active, cfg.tick_seconds);
            s.clone()
        };

        if let Err(e) = snapshot.save_atomic(&cfg.state_path) {
            warn!(error = %e, "state save failed");
        }
    }
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
