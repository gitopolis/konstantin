//! `screentimed` — privileged daemon for enforcing daily screen-time limits.
//!
//! v1 scope (phases 1–5):
//!   * load TOML config
//!   * bind a Unix socket and accept connections
//!   * authenticate clients via `getpeereid(2)`
//!   * enumerate console sessions via `utmpx` and increment per-user
//!     counters every `tick_seconds`, persisting to `state.json`
//!   * scheduled local-midnight reset task (recomputes the boundary
//!     after every reset to stay correct across DST transitions)
//!   * `Subscribe` push channel — server pushes `StatusUpdate` on every
//!     tick and on midnight rollover
//!   * enforcement: `launchctl bootout user/<uid>` when a configured user
//!     hits their daily limit, gated by `enforcement = "logout"` and the
//!     kill-switch file
//!   * answer `GetStatus` with real used / remaining seconds and a
//!     resolved `SessionState`

mod config;
mod enforcement;
mod ipc;
mod sessions;
mod state;
mod time;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Buffer depth for the tick → subscribers broadcast. One entry per tick is
/// generous: a subscriber that lags more than this gets a `Lagged` event
/// (handled by sending a fresh snapshot). 64 covers > 5 min at the default
/// 5 s tick.
const TICK_BROADCAST_CAPACITY: usize = 64;

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

    // Wakeup channel: ticker + midnight resetter publish, every Subscribe
    // connection holds a Receiver. The initial Receiver here is dropped
    // immediately — `Sender::subscribe` on the server side mints fresh
    // ones per connection.
    let (tick_tx, _) = broadcast::channel::<()>(TICK_BROADCAST_CAPACITY);

    let server = ipc::Server::bind(cfg.clone(), state.clone(), tick_tx.clone())
        .await
        .context("binding IPC socket")?;

    let ticker_state = state.clone();
    let ticker_cfg = cfg.clone();
    let ticker_tx = tick_tx.clone();
    tokio::spawn(async move {
        run_ticker(ticker_cfg, ticker_state, ticker_tx).await;
    });

    let resetter_state = state.clone();
    let resetter_cfg = cfg.clone();
    let resetter_tx = tick_tx.clone();
    tokio::spawn(async move {
        run_midnight_resetter(resetter_cfg, resetter_state, resetter_tx).await;
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

/// Sleep until the next local midnight, fire the day rollover, persist,
/// recompute. Recomputing each iteration (rather than adding 86400 s) is
/// what keeps us correct across DST.
async fn run_midnight_resetter(
    cfg: config::Config,
    state: Arc<Mutex<state::State>>,
    tick_tx: broadcast::Sender<()>,
) {
    loop {
        let now = chrono::Local::now();
        let target = time::next_local_midnight();
        let wait = (target - now).to_std().unwrap_or(Duration::from_secs(60));
        info!(
            target = %target.format("%Y-%m-%d %H:%M:%S %z"),
            wait_s = wait.as_secs(),
            "midnight resetter sleeping"
        );
        tokio::time::sleep(wait).await;

        let today = chrono::Local::now().date_naive();
        let (rolled, snapshot) = {
            let mut s = state.lock().expect("state mutex");
            let rolled = s.reset_if_new_day(today);
            if rolled {
                info!(%today, "counters reset (midnight task)");
            } else {
                tracing::debug!(%today, "midnight task: state was already on today");
            }
            (rolled, s.clone())
        };
        if let Err(e) = snapshot.save_atomic(&cfg.state_path) {
            warn!(error = %e, "state save failed (midnight task)");
        }
        // Push fresh snapshots to live subscribers so the tray sees the
        // jump from "23h59m used" to "0s used" without waiting a full tick.
        if rolled {
            let _ = tick_tx.send(());
        }
    }
}

/// Periodically: enumerate console sessions, advance counters, persist,
/// wake any active subscribers, and run an enforcement pass.
async fn run_ticker(
    cfg: config::Config,
    state: Arc<Mutex<state::State>>,
    tick_tx: broadcast::Sender<()>,
) {
    let period = Duration::from_secs(cfg.tick_seconds as u64);
    let mut iv = tokio::time::interval(period);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick of `interval` completes immediately; consume it so the
    // first counter increment happens after a real `period` has elapsed.
    iv.tick().await;

    let mut enforcer = enforcement::Enforcer::new();

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
        // `send` returns Err only when there are no receivers; that's the
        // common case and not an error.
        let _ = tick_tx.send(());

        // Enforcement runs *after* persist + broadcast so a kicked user's
        // last counter value is durable, and any live subscriber sees the
        // pre-kick state before the connection drops.
        enforcer.step(&cfg, &active, &snapshot.counters).await;
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
