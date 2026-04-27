//! `screentime-tray` — per-user menu-bar app for macOS.
//!
//! Shows remaining daily time in `NSStatusItem`. Subscribes to the daemon's
//! push channel so the title updates every tick (and on midnight rollover)
//! without polling. Reconnects automatically if the daemon stops.
//!
//! Threading model:
//!   * **Main thread** — runs `NSApplication`, owns the `NSStatusItem`,
//!     installs an `NSTimer` block (5 Hz) that drains the latest
//!     `UserStatus` from a shared `Mutex` and updates the item title.
//!   * **Worker thread** — runs a current-thread tokio runtime that
//!     opens a `Subscription` and pushes each `UserStatus` into the
//!     shared mutex. On disconnect, sleeps `RECONNECT_BACKOFF` and tries
//!     again.
//!
//! The polling (vs. `dispatch_async` from worker → main) is deliberate:
//! `Retained<NSStatusItem>` is `!Send`, and 200 ms of latency on a
//! status-item title is invisible. Easy to swap later if needed.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("screentime-tray: macOS only");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    imp::main()
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::Result;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::sel;
    use objc2_app_kit::{
        NSApplication, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    // `NSApplicationActivationPolicy` is declared in the
    // NSRunningApplication header, not NSApplication's.
    use objc2_app_kit::NSApplicationActivationPolicy;
    use objc2_foundation::{MainThreadMarker, NSString, NSTimer};
    use screentime_proto::{SessionState, UserStatus};
    use screentime_tray::notifications::{self, NotifTracker};
    use screentime_tray::{default_socket_path, format_remaining, Subscription};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tracing_subscriber::EnvFilter;

    /// How often the main-thread drain timer reads from the worker. 5 Hz
    /// keeps CPU cost negligible while staying well below the daemon's
    /// 5 s default tick.
    const DRAIN_HZ: f64 = 5.0;

    /// Backoff between reconnection attempts when the daemon is
    /// unreachable.
    const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

    /// Shared state between the worker and the main-thread timer.
    #[derive(Default)]
    struct Latest {
        /// Most recent `UserStatus` from the daemon. `None` until the
        /// first frame arrives. Drained by the timer (it `take`s).
        pending: Option<UserStatus>,
        /// True if the worker has lost connectivity. Lets the timer show
        /// a "?" placeholder instead of stale data.
        disconnected: bool,
    }

    pub fn main() -> Result<()> {
        install_tracing();

        let mtm = MainThreadMarker::new()
            .expect("screentime-tray must be launched on the main thread");

        let app = NSApplication::sharedApplication(mtm);
        // Accessory: menu-bar item only — no Dock icon, no main menu.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let status_item = build_status_item(mtm);
        let latest = Arc::new(Mutex::new(Latest::default()));

        // Initial title before any update arrives.
        set_title(&status_item, "screentime: …", mtm);

        spawn_subscriber(latest.clone());
        install_drain_timer(status_item, latest);

        // Blocks until `terminate:` is called from the menu.
        app.run();
        Ok(())
    }

    fn build_status_item(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);

        let menu = NSMenu::new(mtm);

        let quit = NSMenuItem::new(mtm);
        quit.setTitle(&NSString::from_str("Quit"));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        // SAFETY: `setAction` is `unsafe` because raw Objective-C selectors
        // are untyped — sending an unrecognized selector to its target
        // would crash. `terminate:` is implemented by `NSApplication`,
        // which is on the responder chain for menu actions.
        unsafe { quit.setAction(Some(sel!(terminate:))) };
        menu.addItem(&quit);
        item.setMenu(Some(&menu));

        item
    }

    fn spawn_subscriber(latest: Arc<Mutex<Latest>>) {
        std::thread::Builder::new()
            .name("screentime-tray-subscriber".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building subscriber tokio runtime");
                rt.block_on(run_subscriber(latest));
            })
            .expect("spawning subscriber thread");
    }

    async fn run_subscriber(latest: Arc<Mutex<Latest>>) {
        let socket = default_socket_path();
        // The tracker is reset on every UserStatus that has a different
        // `resets_at`, so a daemon restart with the same day's resets_at
        // doesn't re-fire notifications. Lives across reconnects.
        let mut notif = NotifTracker::new();
        loop {
            let mut sub = match Subscription::open(&socket).await {
                Ok(s) => {
                    tracing::info!(path = %socket.display(), "subscribed to daemon");
                    latest.lock().expect("latest").disconnected = false;
                    s
                }
                Err(e) => {
                    tracing::warn!(error = %e, "subscribe open failed; retrying");
                    latest.lock().expect("latest").disconnected = true;
                    tokio::time::sleep(RECONNECT_BACKOFF).await;
                    continue;
                }
            };
            loop {
                match sub.next_update().await {
                    Ok(Some(s)) => {
                        if let Some(minutes) = notif.evaluate(&s) {
                            tracing::info!(minutes, "firing threshold notification");
                            // Fire-and-forget; an osascript hiccup must
                            // not stall the subscribe loop.
                            tokio::spawn(async move {
                                if let Err(e) = notifications::show(minutes).await {
                                    tracing::warn!(error = %e, "notification dispatch failed");
                                }
                            });
                        }
                        latest.lock().expect("latest").pending = Some(s);
                    }
                    Ok(None) => {
                        tracing::warn!("daemon closed connection; reconnecting");
                        latest.lock().expect("latest").disconnected = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "subscribe read error; reconnecting");
                        latest.lock().expect("latest").disconnected = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(RECONNECT_BACKOFF).await;
        }
    }

    fn install_drain_timer(status_item: Retained<NSStatusItem>, latest: Arc<Mutex<Latest>>) {
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            // Block fires on the main thread (run loop where the timer was
            // scheduled), so we can re-derive the marker safely.
            let mtm = MainThreadMarker::new()
                .expect("drain timer must fire on the main thread");
            let drained = {
                let mut g = latest.lock().expect("latest mutex");
                (g.pending.take(), g.disconnected)
            };
            match drained {
                (Some(status), _) => apply_status(&status_item, &status, mtm),
                (None, true) => set_title(&status_item, "screentime: ?", mtm),
                // Nothing new and connection is fine — leave title alone.
                (None, false) => {}
            }
        });

        let interval = 1.0 / DRAIN_HZ;
        // SAFETY: this scheduling API is `unsafe` because the block can in
        // principle be called with a different signature than declared.
        // Our block matches `NonNull<NSTimer>`, the block is heap-allocated
        // by `RcBlock`, and the run loop retains the returned timer for us.
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(interval, true, &block);
        }
    }

    fn apply_status(item: &NSStatusItem, status: &UserStatus, mtm: MainThreadMarker) {
        let label = match status.state {
            SessionState::NotConfigured => "—".to_string(),
            SessionState::Offline => "offline".to_string(),
            SessionState::LimitReached => "0s".to_string(),
            SessionState::Active => format_remaining(status.remaining_seconds),
            SessionState::Paused => {
                format!("⏸ {}", format_remaining(status.remaining_seconds))
            }
        };
        set_title(item, &label, mtm);
    }

    fn set_title(item: &NSStatusItem, title: &str, mtm: MainThreadMarker) {
        if let Some(button) = item.button(mtm) {
            button.setTitle(&NSString::from_str(title));
        }
    }

    fn install_tracing() {
        let filter = EnvFilter::try_from_env("SCREENTIME_TRAY_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,screentime_tray=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    }
}
