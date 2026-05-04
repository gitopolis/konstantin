//! `konstantin-tray` — per-user menu-bar app for macOS.
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
    eprintln!("konstantin-tray: macOS only");
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
        NSAlert, NSApplication, NSCellImagePosition, NSColor, NSImage, NSImageSymbolConfiguration,
        NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    // `NSApplicationActivationPolicy` is declared in the
    // NSRunningApplication header, not NSApplication's.
    use konstantin_proto::admin::{
        AdminRequest, AdminResponse, TrayAutostartChange, TrayAutostartProbe, TrayAutostartState,
        UpdateInstallResult,
    };
    use konstantin_proto::{SessionState, UserStatus};
    use konstantin_tray::admin_xpc::AdminClient;
    use konstantin_tray::notifications::{self, NotifTracker};
    use konstantin_tray::{default_socket_path, format_remaining, Subscription};
    use objc2_app_kit::NSApplicationActivationPolicy;
    use objc2_foundation::{MainThreadMarker, NSString, NSTimer};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tracing_subscriber::EnvFilter;

    /// How often the main-thread drain timer reads from the worker. 5 Hz
    /// keeps CPU cost negligible while staying well below the daemon's
    /// 5 s default tick.
    const DRAIN_HZ: f64 = 5.0;

    /// Backoff between reconnection attempts when the daemon is
    /// unreachable.
    const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

    /// Shared state between the worker and the main-thread timer.
    struct Latest {
        /// Most recent `UserStatus` from the daemon. `None` until the
        /// first frame arrives. Drained by the timer (it `take`s).
        pending: Option<UserStatus>,
        /// True iff we don't currently have an open subscription. We
        /// start in this state and the worker flips it to `false` once
        /// it actually reaches the daemon.
        disconnected: bool,
        /// A release found by an earlier "Check for Updates…" click
        /// that the user deferred via "Later". When `Some`, the
        /// updates menu item is morphed to "Update to <version>" and
        /// clicking it goes straight into install. Cleared once the
        /// update is consumed.
        pending_update: Option<update::Release>,
        /// Last admin-XPC view of `/etc/screentimed/disable` (or the
        /// configured `kill_switch_path`). `None` until the operator
        /// uses the Pause/Unpause action in this process.
        enforcement_paused: Option<bool>,
        enforcement_refreshing: bool,
        enforcement_last_refresh: Option<Instant>,
    }

    impl Default for Latest {
        fn default() -> Self {
            // Start "disconnected" so the UI shows the muted clock
            // honestly until the worker confirms it can reach the daemon.
            Self {
                pending: None,
                disconnected: true,
                pending_update: None,
                enforcement_paused: None,
                enforcement_refreshing: false,
                enforcement_last_refresh: None,
            }
        }
    }

    /// All the long-lived AppKit handles the drain timer needs to
    /// touch each tick. Built once by `build_status_item`, moved into
    /// the timer block which the run loop retains for the app's
    /// lifetime.
    struct Tray {
        status_item: Retained<NSStatusItem>,
        pause_enforcement_item: Retained<NSMenuItem>,
        reload_item: Retained<NSMenuItem>,
        updates_item: Retained<NSMenuItem>,
    }

    /// Shared `Arc<Mutex<Latest>>` set during `main()` so action
    /// handlers (which can't be parameterised through the
    /// `define_class!` macro without ivars) can read pending updates.
    /// `OnceLock` so it's a fail-loud configuration mistake to install
    /// it twice.
    static LATEST: std::sync::OnceLock<Arc<Mutex<Latest>>> = std::sync::OnceLock::new();

    pub fn main() -> Result<()> {
        install_tracing();

        // Log path-resolution mode early. Useful when a bug report
        // mentions install paths — at a glance you know whether the
        // user is running the production .app bundle or somebody's
        // dev tree.
        match bundle::Paths::resolve() {
            Ok(p) => tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                arch = std::env::consts::ARCH,
                source = p.source.label(),
                daemon = %p.daemon_binary.display(),
                "konstantin-tray starting"
            ),
            Err(e) => tracing::warn!(error = %e, "could not resolve bundle paths"),
        }

        let mtm =
            MainThreadMarker::new().expect("konstantin-tray must be launched on the main thread");

        let app = NSApplication::sharedApplication(mtm);
        // Accessory: menu-bar item only — no Dock icon, no main menu.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Controller owns the target/action handlers for menu items.
        // Bound here so it lives until `app.run()` returns (process
        // exit). Menu items hold a weak reference per Cocoa convention.
        let controller = actions::Controller::new(mtm);
        let tray = build_status_item(mtm, &controller);
        let latest = Arc::new(Mutex::new(Latest::default()));
        // Publish for action handlers. `set` returns Err if the slot
        // was already filled — `main()` runs once, so this is fail-fast
        // diagnostic for an accidental double-call.
        let _ = LATEST.set(latest.clone());

        // Initial visuals before any update arrives — match the default
        // `disconnected: true` state (muted clock, no time label).
        apply_visual(&tray.status_item, true, "", mtm);

        // Idempotent: write our per-user LaunchAgent plist so launchd
        // auto-starts the tray on next login. Doesn't bootstrap — we
        // ARE the running tray; bootstrap would race-spawn a sibling.
        //
        // Skipped when running from a dev tree, since the LaunchAgent
        // would point at `target/release/konstantin-tray`; if that
        // binary is later cleaned (`cargo clean`) or moved, login
        // auto-start would silently fail.
        match bundle::Paths::resolve().map(|p| p.source) {
            Ok(bundle::Source::Bundle) => {
                if let Err(e) = install::ensure_user_launchagent() {
                    tracing::warn!(error = %e, "user LaunchAgent setup failed (non-fatal)");
                }
            }
            Ok(bundle::Source::DevTree) => {
                tracing::info!("dev-tree run — skipping user LaunchAgent rewrite");
            }
            Err(_) => {} // already logged above
        }

        // First-launch flow: if we can't reach the daemon AND no system
        // plist is present, ask for admin auth and run the privileged
        // install. If the user cancels, exit cleanly.
        if !install::daemon_socket_reachable()
            && !install::system_plist_present()
            && !install::run_first_launch_install(mtm)
        {
            tracing::info!("first-launch setup not completed; quitting");
            return Ok(());
        }

        // Ask for notification permission once at startup. macOS
        // remembers the answer per-bundle, so subsequent launches
        // don't re-prompt. Doing it here means the TCC sheet appears
        // immediately rather than at the moment a threshold actually
        // fires (which would be jarring).
        notifications::request_authorization();

        spawn_subscriber(latest.clone());
        install_drain_timer(tray, latest);

        // Blocks until `terminate:` is called from the menu.
        app.run();
        Ok(())
    }

    fn build_status_item(mtm: MainThreadMarker, controller: &actions::Controller) -> Tray {
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);

        let menu = NSMenu::new(mtm);
        // Disable AppKit's auto-validation so our explicit `setEnabled`
        // calls in the drain timer are authoritative.
        menu.setAutoenablesItems(false);

        let pause_enforcement_item = make_action_item(
            mtm,
            "Pause Enforcement",
            sel!(toggleEnforcement:),
            controller,
        );
        let reload_item = make_action_item(
            mtm,
            "Reload Configuration",
            sel!(reloadConfiguration:),
            controller,
        );

        // Initial enable-state matches the default `disconnected: true`
        // — daemon-mediated controls wait until the worker reports back.
        pause_enforcement_item.setEnabled(false);
        reload_item.setEnabled(false);

        menu.addItem(&pause_enforcement_item);
        menu.addItem(&reload_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let configure = make_action_item(mtm, "Configure…", sel!(configure:), controller);
        let log = make_action_item(mtm, "Open Log", sel!(openLog:), controller);
        // These two are always actionable — no daemon-state dependency.
        configure.setEnabled(true);
        log.setEnabled(true);
        menu.addItem(&configure);
        menu.addItem(&log);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Updater entry. Title morphs to "Update to <version>" once a
        // check has surfaced a newer release that the user deferred
        // via "Later" — drain timer drives the morph from
        // `Latest::pending_update`.
        let updates_item = make_action_item(
            mtm,
            "Check for Updates…",
            sel!(checkForUpdates:),
            controller,
        );
        updates_item.setEnabled(true);
        menu.addItem(&updates_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let uninstall = make_action_item(mtm, "Uninstall…", sel!(uninstall:), controller);
        uninstall.setEnabled(true);
        menu.addItem(&uninstall);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = NSMenuItem::new(mtm);
        quit.setTitle(&NSString::from_str("Quit"));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        // SAFETY: `setAction` is `unsafe` because raw Objective-C selectors
        // are untyped — sending an unrecognized selector to its target
        // would crash. `terminate:` is implemented by `NSApplication`,
        // which is on the responder chain for menu actions.
        unsafe { quit.setAction(Some(sel!(terminate:))) };
        quit.setEnabled(true);
        menu.addItem(&quit);

        item.setMenu(Some(&menu));

        Tray {
            status_item: item,
            pause_enforcement_item,
            reload_item,
            updates_item,
        }
    }

    /// Build a menu item wired to a selector on `controller`. Returns
    /// the retained item so the caller can hold it for later
    /// state updates.
    fn make_action_item(
        mtm: MainThreadMarker,
        title: &str,
        action: objc2::runtime::Sel,
        controller: &actions::Controller,
    ) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        // SAFETY: same as `setAction` above. Selectors here are
        // declared on `Controller` via `define_class!`, and we set the
        // target to a controller of that class — the dispatch is
        // type-correct at runtime.
        unsafe {
            item.setAction(Some(action));
            item.setTarget(Some(controller));
        }
        item
    }

    fn spawn_subscriber(latest: Arc<Mutex<Latest>>) {
        std::thread::Builder::new()
            .name("konstantin-tray-subscriber".into())
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
                    let mut g = latest.lock().expect("latest");
                    g.disconnected = false;
                    g.enforcement_paused = None;
                    g.enforcement_last_refresh = None;
                    drop(g);
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
                            // Fire-and-forget; a notification hiccup must
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

    fn install_drain_timer(tray: Tray, latest: Arc<Mutex<Latest>>) {
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            // Block fires on the main thread (run loop where the timer was
            // scheduled), so we can re-derive the marker safely.
            let mtm = MainThreadMarker::new().expect("drain timer must fire on the main thread");
            let (pending, disconnected, pending_update_version, enforcement_paused) = {
                let mut g = latest.lock().expect("latest mutex");
                (
                    g.pending.take(),
                    g.disconnected,
                    g.pending_update.as_ref().map(|r| r.version.to_string()),
                    g.enforcement_paused,
                )
            };

            // Menu enable-state. Idempotent — `setEnabled` with the
            // current value is a no-op in AppKit, so calling every
            // tick is fine.
            tray.pause_enforcement_item.setEnabled(!disconnected);
            tray.reload_item.setEnabled(!disconnected);
            let pause_title = match enforcement_paused {
                Some(true) => "Unpause Enforcement",
                _ => "Pause Enforcement",
            };
            tray.pause_enforcement_item
                .setTitle(&NSString::from_str(pause_title));
            maybe_refresh_enforcement_state(latest.clone(), disconnected);

            // Updater menu item morph. AppKit's `setTitle` is also
            // idempotent — comparing first would just be book-keeping.
            let updates_title = match &pending_update_version {
                Some(v) => format!("Update to {v}"),
                None => "Check for Updates…".to_string(),
            };
            tray.updates_item
                .setTitle(&NSString::from_str(&updates_title));

            // Visuals. The muted clock trumps any pending status if
            // we're currently disconnected — even if a stale `pending`
            // is sitting around, the daemon is unreachable *now*.
            if disconnected {
                apply_visual(&tray.status_item, true, "", mtm);
            } else if let Some(status) = pending {
                apply_visual(&tray.status_item, false, &status_label(&status), mtm);
            }
            // else: connected, no fresh status — leave visuals alone.
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

    const ENFORCEMENT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

    fn maybe_refresh_enforcement_state(latest: Arc<Mutex<Latest>>, disconnected: bool) {
        if disconnected {
            return;
        }

        let should_spawn = {
            let mut g = latest.lock().expect("latest mutex");
            if g.enforcement_refreshing {
                false
            } else {
                let due = g
                    .enforcement_last_refresh
                    .map(|last| last.elapsed() >= ENFORCEMENT_REFRESH_INTERVAL)
                    .unwrap_or(true);
                if due {
                    g.enforcement_refreshing = true;
                    true
                } else {
                    false
                }
            }
        };

        if !should_spawn {
            return;
        }

        std::thread::Builder::new()
            .name("konstantin-enforcement-refresh".into())
            .spawn(move || {
                let refreshed = query_enforcement_paused();
                let mut g = latest.lock().expect("latest mutex");
                g.enforcement_refreshing = false;
                g.enforcement_last_refresh = Some(Instant::now());
                match refreshed {
                    Ok(paused) => {
                        g.enforcement_paused = Some(paused);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "enforcement state refresh failed");
                    }
                }
            })
            .expect("spawning enforcement refresh thread");
    }

    fn query_enforcement_paused() -> Result<bool> {
        match AdminClient::send(AdminRequest::GetEnforcementState)? {
            AdminResponse::EnforcementState { paused, .. } => Ok(paused),
            AdminResponse::Unauthorized { reason } => anyhow::bail!("Not authorized: {reason}"),
            AdminResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected admin response: {other:?}"),
        }
    }

    fn status_label(status: &UserStatus) -> String {
        match status.state {
            // No limit configured for this account — clock glyph alone
            // is enough; an em-dash next to it just looks like noise.
            SessionState::NotConfigured => String::new(),
            SessionState::Offline => "offline".to_string(),
            SessionState::LimitReached => "0s".to_string(),
            SessionState::Active => format_remaining(status.remaining_seconds),
            SessionState::Paused => {
                format!("⏸ {}", format_remaining(status.remaining_seconds))
            }
        }
    }

    /// Render the status item's icon + title.
    ///
    /// The icon is the `clock` SF Symbol. When connected, it's marked
    /// as a template image so the menu bar tints it the right color
    /// for light/dark mode. When `disconnected`, we bake `secondaryLabelColor`
    /// into the symbol via an `NSImageSymbolConfiguration` and drop the
    /// template flag — `NSStatusBarButton` ignores `contentTintColor`
    /// for template images, so we have to encode the gray in the image
    /// itself.
    fn apply_visual(item: &NSStatusItem, disconnected: bool, label: &str, mtm: MainThreadMarker) {
        let Some(button) = item.button(mtm) else {
            return;
        };

        let symbol = NSString::from_str("clock");
        if let Some(base) =
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol, None)
        {
            let image = if disconnected {
                let cfg = NSImageSymbolConfiguration::configurationWithHierarchicalColor(
                    &NSColor::secondaryLabelColor(),
                );
                let tinted = base.imageWithSymbolConfiguration(&cfg).unwrap_or(base);
                // Non-template so the menu bar uses our embedded color
                // instead of overriding it with the menu-bar foreground.
                tinted.setTemplate(false);
                tinted
            } else {
                base.setTemplate(true);
                base
            };
            button.setImage(Some(&image));
        }
        button.setImagePosition(NSCellImagePosition::ImageLeading);
        button.setTitle(&NSString::from_str(label));
    }

    fn install_tracing() {
        let filter = EnvFilter::try_from_env("KONSTANTIN_TRAY_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,konstantin_tray=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    }

    /// Click handler for the updates menu item. If a previous check
    /// has already surfaced a release that the user deferred, jump
    /// straight into the install flow with that release. Otherwise
    /// run the check.
    fn check_for_updates_flow(mtm: MainThreadMarker) {
        let paths = match bundle::Paths::resolve() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "could not resolve bundle paths for update");
                alerts::message(
                    mtm,
                    "Couldn't Check for Updates",
                    &format!("Couldn't locate the running bundle: {e}"),
                );
                return;
            }
        };

        // Take any pending release out so a successful install doesn't
        // leave it stale; if the user cancels the admin sheet it'll get
        // restashed below.
        let pending = LATEST
            .get()
            .and_then(|l| l.lock().ok().and_then(|mut g| g.pending_update.take()));

        if let Some(release) = pending {
            update::run_install_flow(mtm, &paths, &release);
            return;
        }

        update::run_check_for_updates_flow(mtm, &paths, |release| {
            // User picked "Later" — stash the release so the menu item
            // morphs to "Update to <version>" on the next drain tick.
            if let Some(latest) = LATEST.get() {
                if let Ok(mut g) = latest.lock() {
                    g.pending_update = Some(release);
                }
            }
        });
    }

    /// Resolves paths to the daemon binary, daemon plist template, and
    /// example config — either from this `.app` bundle's
    /// `Contents/Resources/` (production) or from `target/<profile>/`
    /// + `packaging/` (developer running `cargo run` or
    /// `target/release/konstantin-tray` directly).
    ///
    /// One source of truth so anyone needing a bundled artifact —
    /// install, future "update daemon" flow, diagnostics — calls
    /// `bundle::Paths::resolve()`.
    mod bundle {
        use std::path::PathBuf;

        /// Where we found the bundled artifacts. Useful for logs and
        /// for telling devs apart from end users.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Source {
            /// Resolved from a real `.app` bundle's `Contents/`.
            Bundle,
            /// Resolved from a workspace `target/<profile>/` plus
            /// `packaging/`. The `cargo run` / `target/release/...`
            /// path.
            DevTree,
        }

        impl Source {
            pub fn label(self) -> &'static str {
                match self {
                    Self::Bundle => "bundle",
                    Self::DevTree => "dev-tree",
                }
            }
        }

        pub struct Paths {
            pub daemon_binary: PathBuf,
            pub daemon_plist: PathBuf,
            pub config_example: PathBuf,
            pub source: Source,
            /// `.app` bundle root (`/Applications/Konstantin.app`-ish)
            /// when running from a real bundle. `None` in dev-tree mode.
            /// Recorded at install time so the daemon can self-uninstall
            /// if the operator drag-to-Trashes the app.
            pub bundle_root: Option<PathBuf>,
        }

        impl Paths {
            /// Try the bundle layout first; fall back to dev-tree
            /// layout. Errors only on a missing/unparented exe path —
            /// either path resolution succeeds or something is very
            /// wrong with the environment.
            pub fn resolve() -> anyhow::Result<Self> {
                let exe = std::env::current_exe()
                    .map_err(|e| anyhow::anyhow!("reading current_exe: {e}"))?;
                let exe_dir = exe
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("exe has no parent dir"))?;

                // Bundle layout: `exe_dir` is `Contents/MacOS/`. If
                // `Contents/Resources/screentimed` exists alongside, we're
                // in a bundle.
                if let Some(contents) = exe_dir.parent() {
                    let resources = contents.join("Resources");
                    let bundled_daemon = resources.join("screentimed");
                    if bundled_daemon.is_file() {
                        let bundle_root = contents.parent().map(PathBuf::from);
                        return Ok(Self {
                            daemon_binary: bundled_daemon,
                            daemon_plist: contents
                                .join("Library/LaunchDaemons/com.gitopolis.screentimed.plist"),
                            config_example: resources.join("config.example.toml"),
                            source: Source::Bundle,
                            bundle_root,
                        });
                    }
                }

                // Dev-tree fallback. `exe_dir` is `target/<profile>/`
                // (`release` or `debug`); the daemon binary lives next
                // to the tray, and `packaging/` lives at the workspace
                // root.
                let profile_dir = exe_dir;
                let workspace = profile_dir
                    .parent()
                    .and_then(|p| p.parent())
                    .ok_or_else(|| {
                        anyhow::anyhow!("can't infer workspace root from {}", exe.display())
                    })?;

                Ok(Self {
                    daemon_binary: profile_dir.join("screentimed"),
                    daemon_plist: workspace.join("packaging/com.gitopolis.screentimed.plist"),
                    config_example: workspace.join("packaging/config.example.toml"),
                    source: Source::DevTree,
                    bundle_root: None,
                })
            }
        }
    }

    /// Custom `NSObject` subclass that owns the menu-item action
    /// handlers. Cocoa target/action menu callbacks need a real Obj-C
    /// class to receive them, so we declare one with `define_class!`
    /// and route each selector to a Rust function.
    ///
    /// All action methods run on the main thread (Cocoa guarantees
    /// this for menu actions), so we can derive a `MainThreadMarker`
    /// inside each handler.
    mod actions {
        use super::*;
        use objc2::define_class;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyObject, NSObject};
        use objc2::MainThreadOnly;

        define_class!(
            #[unsafe(super(NSObject))]
            #[thread_kind = MainThreadOnly]
            #[name = "KonstantinTrayController"]
            pub struct Controller;

            impl Controller {
                #[unsafe(method(toggleEnforcement:))]
                fn toggle_enforcement_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    toggle_enforcement(mtm);
                }

                #[unsafe(method(reloadConfiguration:))]
                fn reload_configuration_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    reload_configuration(mtm);
                }

                #[unsafe(method(configure:))]
                fn configure_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    super::config_ui::open(mtm);
                }

                #[unsafe(method(openLog:))]
                fn open_log_action(&self, _sender: Option<&AnyObject>) {
                    open_log();
                }

                #[unsafe(method(checkForUpdates:))]
                fn check_for_updates_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    super::check_for_updates_flow(mtm);
                }

                #[unsafe(method(uninstall:))]
                fn uninstall_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    uninstall_flow(mtm);
                }
            }
        );

        impl Controller {
            pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
                let alloc = Self::alloc(mtm);
                unsafe { objc2::msg_send![alloc, init] }
            }
        }

        fn toggle_enforcement(mtm: MainThreadMarker) {
            let outcome = progress::run_with_panel(
                mtm,
                "Updating Enforcement",
                "Updating Konstantin enforcement…",
                "konstantin-tray-admin-xpc",
                || -> Result<bool> {
                    let paused = query_enforcement_paused()?;
                    let target = !paused;
                    let changed =
                        AdminClient::send(AdminRequest::SetEnforcementPaused { paused: target })?;
                    match changed {
                        AdminResponse::EnforcementState { paused, .. } => Ok(paused),
                        AdminResponse::Unauthorized { reason } => {
                            anyhow::bail!("Not authorized: {reason}");
                        }
                        AdminResponse::Error { message } => anyhow::bail!("{message}"),
                        other => anyhow::bail!("unexpected admin response: {other:?}"),
                    }
                },
            );

            match outcome {
                Ok(paused) => {
                    if let Some(latest) = LATEST.get() {
                        latest.lock().expect("latest").enforcement_paused = Some(paused);
                    }
                }
                Err(e) => {
                    alerts::message(mtm, "Couldn't update enforcement.", &e.to_string());
                }
            }
        }

        fn reload_configuration(mtm: MainThreadMarker) {
            let outcome = progress::run_with_panel(
                mtm,
                "Reloading Configuration",
                "Reloading Konstantin configuration…",
                "konstantin-tray-admin-xpc",
                || -> Result<()> {
                    match AdminClient::send(AdminRequest::ReloadDaemon)? {
                        AdminResponse::Ok => Ok(()),
                        AdminResponse::Unauthorized { reason } => {
                            anyhow::bail!("Not authorized: {reason}");
                        }
                        AdminResponse::Error { message } => anyhow::bail!("{message}"),
                        other => anyhow::bail!("unexpected admin response: {other:?}"),
                    }
                },
            );

            match outcome {
                Ok(()) => {}
                Err(e) => {
                    alerts::message(mtm, "Couldn't reload configuration.", &e.to_string());
                }
            }
        }

        /// `Open Log` — hand the file off to Console.app (the macOS
        /// default for .log files). No admin needed; the file is
        /// world-readable by default since the daemon's launchd plist
        /// uses standard `StandardOutPath` redirection.
        fn open_log() {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("/var/log/screentimed.log")
                .status();
        }

        /// Uninstall flow:
        ///   1. Confirm with the user (destructive).
        ///   2. Ask the root daemon over admin XPC to tear down the
        ///      system install plus every per-user tray LaunchAgent plist.
        ///   3. Tell the user, then terminate.
        ///
        /// The daemon skips booting out the operator's own tray so this
        /// process can render the success alert and quit itself. Other
        /// users' trays are booted out immediately.
        ///
        /// `/etc/screentimed/` (config) is intentionally preserved so a
        /// reinstall resumes with the user's settings. `/var/db/screentimed/`
        /// (counter state) *is* removed — uninstall means uninstall.
        /// The Homebrew cask's `zap` block at `packaging/konstantin.rb`
        /// also removes the config directory for users who want a
        /// clean wipe.
        fn uninstall_flow(mtm: MainThreadMarker) {
            if !alerts::confirm(
                mtm,
                "Uninstall Konstantin?",
                "Stops the background service and removes its files, \
                 including saved counter state.\n\n\
                 Your configuration (/etc/screentimed/) is preserved so \
                 a reinstall picks up your settings. To remove that too, \
                 run `brew uninstall --zap konstantin` after this \
                 finishes, or delete it by hand.",
                "Uninstall",
                "Cancel",
            ) {
                return;
            }

            let outcome = progress::run_with_panel(
                mtm,
                "Uninstalling Konstantin",
                "Stopping the background service and removing files…",
                "konstantin-tray-uninstall",
                || {
                    AdminClient::send(AdminRequest::Uninstall {
                        preserve_config: true,
                    })
                },
            );

            match outcome {
                Ok(AdminResponse::Ok) => {}
                Ok(AdminResponse::Unauthorized { reason }) => {
                    alerts::message(mtm, "Administrator required.", &reason);
                    return;
                }
                Ok(AdminResponse::Error { message }) => {
                    alerts::message(mtm, "Couldn't uninstall Konstantin.", &message);
                    return;
                }
                Ok(other) => {
                    alerts::message(
                        mtm,
                        "Couldn't uninstall Konstantin.",
                        &format!("Unexpected daemon response: {other:?}"),
                    );
                    return;
                }
                Err(e) => {
                    alerts::message(mtm, "Couldn't uninstall Konstantin.", &e.to_string());
                    return;
                }
            }

            alerts::message(
                mtm,
                "Konstantin has been uninstalled.",
                "The app will now quit. Move Konstantin.app to the Trash \
                 to finish removing the application bundle.",
            );

            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }

    /// Two-button (`primary` / `secondary`) and one-button (`OK`) NSAlert
    /// helpers. Used by anything that wants a quick modal — the install
    /// flow, menu actions, error reporting.
    mod alerts {
        use super::*;
        use objc2::rc::Retained;

        /// Show a confirm alert with two buttons. Returns `true` when the
        /// user clicks `primary`, `false` for `secondary`.
        pub fn confirm(
            mtm: MainThreadMarker,
            title: &str,
            body: &str,
            primary: &str,
            secondary: &str,
        ) -> bool {
            let alert = make(mtm, title, body);
            alert.addButtonWithTitle(&NSString::from_str(primary));
            alert.addButtonWithTitle(&NSString::from_str(secondary));
            // NSAlertFirstButtonReturn = 1000.
            alert.runModal() == 1000
        }

        /// Show an informational alert with a single OK button. Blocks
        /// until the user dismisses.
        pub fn message(mtm: MainThreadMarker, title: &str, body: &str) {
            let alert = make(mtm, title, body);
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }

        fn make(mtm: MainThreadMarker, message: &str, informative: &str) -> Retained<NSAlert> {
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(message));
            alert.setInformativeText(&NSString::from_str(informative));
            alert
        }
    }

    /// Generic "show a spinner panel while a closure runs on a worker
    /// thread" primitive. Decoupled from any specific kind of work —
    /// it just owns the panel + run-loop pump.
    mod progress {
        use super::*;
        use objc2::rc::Retained;
        use objc2::MainThreadOnly;
        use objc2_app_kit::{
            NSBackingStoreType, NSPanel, NSProgressIndicator, NSProgressIndicatorStyle,
            NSTextField, NSWindowStyleMask,
        };
        use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize};
        use std::sync::mpsc;

        /// Show a titled NSPanel with an indeterminate spinner and a
        /// one-liner status message. Spawns `work` on a background
        /// thread named `thread_name`, pumps the main run loop in 50 ms
        /// slices so the panel animates and the cursor stays normal,
        /// and returns the closure's value once it sends. Closes the
        /// panel before returning. Must be called from the main thread.
        pub fn run_with_panel<T: Send + 'static>(
            mtm: MainThreadMarker,
            panel_title: &str,
            panel_message: &str,
            thread_name: &str,
            work: impl FnOnce() -> T + Send + 'static,
        ) -> T {
            let panel = build_panel(mtm, panel_title, panel_message);
            // `orderFrontRegardless` brings the window forward even when
            // the app is `Accessory` (no Dock presence). Combined with
            // `activate`, it puts the panel front-and-centre after any
            // OS-level prompt dismisses.
            panel.orderFrontRegardless();
            NSApplication::sharedApplication(mtm).activate();

            let (tx, rx) = mpsc::channel::<T>();
            let thread_label = thread_name.to_string();
            std::thread::Builder::new()
                .name(thread_name.into())
                .spawn(move || {
                    // Catch panics so a crashed worker leaves a log
                    // line instead of vanishing silently. The channel
                    // still ends up disconnected (we don't have a `T`
                    // to send), so `pump_run_loop_until` will still
                    // notice — but at least the cause is in the log.
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
                        Ok(value) => {
                            let _ = tx.send(value);
                        }
                        Err(payload) => {
                            let msg = panic_payload_message(&payload);
                            tracing::error!(
                                thread = %thread_label,
                                panic = %msg,
                                "worker thread panicked"
                            );
                        }
                    }
                })
                .expect("spawn worker thread");

            let result = pump_run_loop_until(&rx);

            panel.close();
            result
        }

        fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
            if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "non-string panic payload".to_string()
            }
        }

        /// Pump the main run loop in 50 ms slices, draining whatever
        /// events are pending each tick (panel redraws, spinner frame
        /// advances) until the worker sends its result.
        fn pump_run_loop_until<T>(rx: &mpsc::Receiver<T>) -> T {
            let run_loop = NSRunLoop::currentRunLoop();
            loop {
                let limit = NSDate::dateWithTimeIntervalSinceNow(0.05);
                unsafe {
                    let _ = run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &limit);
                }
                match rx.try_recv() {
                    Ok(v) => return v,
                    Err(mpsc::TryRecvError::Empty) => continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        unreachable!("worker thread vanished without sending result")
                    }
                }
            }
        }

        fn build_panel(mtm: MainThreadMarker, title: &str, message: &str) -> Retained<NSPanel> {
            let content_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(380.0, 110.0));
            let style = NSWindowStyleMask::Titled;
            let backing = NSBackingStoreType::Buffered;

            let panel: Retained<NSPanel> = unsafe {
                let alloc = NSPanel::alloc(mtm);
                objc2::msg_send![
                    alloc,
                    initWithContentRect: content_rect,
                    styleMask: style.0,
                    backing: backing.0,
                    defer: false,
                ]
            };
            panel.setTitle(&NSString::from_str(title));
            panel.center();

            let content = panel.contentView().expect("panel content view");

            // Spinning indeterminate progress indicator.
            let spinner_rect = NSRect::new(NSPoint::new(20.0, 38.0), NSSize::new(32.0, 32.0));
            let spinner = NSProgressIndicator::new(mtm);
            spinner.setFrame(spinner_rect);
            unsafe {
                spinner.setStyle(NSProgressIndicatorStyle::Spinning);
                spinner.setIndeterminate(true);
                spinner.setUsesThreadedAnimation(true);
            }
            content.addSubview(&spinner);
            unsafe { spinner.startAnimation(None) };

            // Status label to the right of the spinner.
            let label_rect = NSRect::new(NSPoint::new(64.0, 30.0), NSSize::new(300.0, 50.0));
            let label = NSTextField::labelWithString(&NSString::from_str(message), mtm);
            label.setFrame(label_rect);
            content.addSubview(&label);

            panel
        }
    }

    /// Privileged-action primitive: run a bash command as root via
    /// `osascript … with administrator privileges`, with a small
    /// progress panel and a responsive main thread. Thin wrapper
    /// around `mod progress` that knows how to invoke osascript and
    /// classify cancel-vs-failure outcomes.
    mod admin {
        use super::*;

        pub enum Error {
            /// User dismissed the OS password prompt.
            Cancelled,
            /// osascript or the underlying command exited non-zero. The
            /// string is the captured stderr.
            Failed(String),
        }

        /// Run `bash_command` as root. Shows a progress panel titled
        /// `panel_title` with the spinner-adjacent message
        /// `panel_message` for the duration. Must be called from the
        /// main thread.
        pub fn run_with_progress(
            mtm: MainThreadMarker,
            panel_title: &str,
            panel_message: &str,
            bash_command: &str,
        ) -> Result<(), Error> {
            let cmd = bash_command.to_string();
            super::progress::run_with_panel(
                mtm,
                panel_title,
                panel_message,
                "konstantin-tray-admin",
                move || run_osascript_blocking(&cmd),
            )
        }

        fn run_osascript_blocking(bash_command: &str) -> Result<(), Error> {
            // AppleScript double-quoted strings escape `\` and `"`. We
            // build single-line bash here so no newline escaping is
            // needed.
            let escaped = bash_command.replace('\\', "\\\\").replace('"', "\\\"");
            let applescript =
                format!(r#"do shell script "{escaped}" with administrator privileges"#);
            let output = std::process::Command::new("/usr/bin/osascript")
                .arg("-e")
                .arg(&applescript)
                .output()
                .map_err(|e| Error::Failed(format!("spawn osascript: {e}")))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            // osascript reports user cancellation as
            // `execution error: User canceled. (-128)`.
            if stderr.contains("User canceled") {
                return Err(Error::Cancelled);
            }
            Err(Error::Failed(stderr.into_owned()))
        }
    }

    mod service_management {
        use super::*;
        use objc2::msg_send;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyClass, AnyObject};
        use objc2_foundation::NSString;
        use std::ptr;

        #[link(name = "ServiceManagement", kind = "framework")]
        extern "C" {}

        const DAEMON_PLIST_NAME: &str = "com.gitopolis.screentimed.plist";

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Status {
            NotRegistered,
            Enabled,
            RequiresApproval,
            NotFound,
            Unknown(isize),
        }

        pub fn daemon_status() -> Result<Status> {
            let service = daemon_service()?;
            let raw: isize = unsafe { msg_send![&*service, status] };
            Ok(Status::from_raw(raw))
        }

        pub fn register_daemon() -> Result<Status> {
            let service = daemon_service()?;
            let mut error: *mut AnyObject = ptr::null_mut();
            let ok: bool = unsafe { msg_send![&*service, registerAndReturnError: &mut error] };
            if !ok {
                let status = daemon_status().unwrap_or(Status::NotFound);
                if matches!(status, Status::Enabled | Status::RequiresApproval) {
                    return Ok(status);
                }
                anyhow::bail!("{}", error_message(error));
            }
            daemon_status()
        }

        pub fn open_login_items_settings() {
            if let Some(cls) = sm_app_service_class() {
                unsafe {
                    let _: () = msg_send![cls, openSystemSettingsLoginItems];
                }
            }
        }

        fn daemon_service() -> Result<Retained<AnyObject>> {
            let cls = sm_app_service_class()
                .ok_or_else(|| anyhow::anyhow!("SMAppService is unavailable"))?;
            let plist = NSString::from_str(DAEMON_PLIST_NAME);
            let service: Option<Retained<AnyObject>> =
                unsafe { msg_send![cls, daemonServiceWithPlistName: &*plist] };
            service.ok_or_else(|| anyhow::anyhow!("SMAppService returned nil daemon service"))
        }

        fn sm_app_service_class() -> Option<&'static AnyClass> {
            AnyClass::get(c"SMAppService")
        }

        fn error_message(error: *mut AnyObject) -> String {
            if error.is_null() {
                return "ServiceManagement registration failed".to_string();
            }
            let desc: Option<Retained<NSString>> =
                unsafe { msg_send![&*error, localizedDescription] };
            desc.map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "ServiceManagement registration failed".to_string())
        }

        impl Status {
            fn from_raw(raw: isize) -> Self {
                match raw {
                    0 => Self::NotRegistered,
                    1 => Self::Enabled,
                    2 => Self::RequiresApproval,
                    3 => Self::NotFound,
                    other => Self::Unknown(other),
                }
            }
        }
    }

    /// In-app updater. Hits `api.github.com/repos/<repo>/releases/latest`,
    /// finds the architecture-matched asset zip, verifies GitHub's
    /// per-asset SHA-256 digest, and asks the privileged daemon to
    /// install the `.app` bundle in place through the detached updater
    /// helper. The helper self-rolls back if the new daemon does not
    /// become reachable.
    ///
    /// Source of truth for the version comparison is
    /// `env!("CARGO_PKG_VERSION")`. CI runs `cargo set-version
    /// --workspace "$VERSION"` before each release build, so the version
    /// baked into a tagged-release binary is always the right one.
    mod update {
        use super::*;
        use semver::Version;
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        use std::path::{Path, PathBuf};
        use std::sync::OnceLock;
        use std::time::Duration;

        const REPO: &str = "gitopolis/konstantin";
        const RELEASES_API: &str =
            "https://api.github.com/repos/gitopolis/konstantin/releases/latest";
        const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

        /// `env!("CARGO_PKG_VERSION")` parsed once. Panics at first call
        /// if the build's version is malformed — that's a bug worth
        /// crashing on.
        pub fn current_version() -> &'static Version {
            static V: OnceLock<Version> = OnceLock::new();
            V.get_or_init(|| {
                Version::parse(env!("CARGO_PKG_VERSION"))
                    .expect("CARGO_PKG_VERSION must be valid semver")
            })
        }

        /// Map `std::env::consts::ARCH` to the `<arch>` token that the
        /// release pipeline embeds in asset filenames. `aarch64` is the
        /// Rust target arch for Apple Silicon; the release matrix labels
        /// that build `arm64`. The eventual x86_64 matrix entry should
        /// be labelled `x86_64` (matches `uname -m` and Homebrew's
        /// bottle-arch convention) — if it lands as `amd64` instead, the
        /// mapping below is the only thing that changes.
        pub fn current_arch_label() -> Option<&'static str> {
            match std::env::consts::ARCH {
                "aarch64" => Some("arm64"),
                "x86_64" => Some("x86_64"),
                _ => None,
            }
        }

        #[derive(Clone, Debug)]
        pub struct Release {
            pub version: Version,
            pub asset_url: String,
            pub asset_name: String,
            /// Lowercase hex SHA-256 of the asset, taken from the
            /// API's `digest` field (the same hash GitHub displays on
            /// the release page). Format on the wire is `sha256:<hex>`;
            /// we strip the prefix and lowercase here.
            pub asset_sha256: String,
        }

        pub enum CheckOutcome {
            UpToDate,
            Newer(Release),
        }

        #[derive(Debug)]
        pub enum Error {
            /// HTTP, DNS, TLS, or transport errors.
            Network(String),
            /// JSON couldn't be parsed, or required fields were missing.
            Parse(String),
            /// No `Konstantin-<version>-<arch>.zip` asset for our arch.
            NoAssetForArch,
            /// Running from a dev tree, or running on an unsupported
            /// architecture — refuse to update.
            UnsupportedEnvironment,
            /// Filesystem error during download / staging.
            Io(String),
        }

        impl std::fmt::Display for Error {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Network(s) => write!(f, "network error: {s}"),
                    Self::Parse(s) => write!(f, "couldn't parse GitHub response: {s}"),
                    Self::NoAssetForArch => write!(
                        f,
                        "no compatible release asset for this architecture"
                    ),
                    Self::UnsupportedEnvironment => write!(
                        f,
                        "updates are only available from an installed .app bundle on a supported architecture"
                    ),
                    Self::Io(s) => write!(f, "i/o error: {s}"),
                }
            }
        }

        // ─── Network ─────────────────────────────────────────────────

        fn user_agent() -> String {
            format!("konstantin-tray/{}", env!("CARGO_PKG_VERSION"))
        }

        /// One-shot agent for HTTP(S). `ureq`'s default agent is fine
        /// for the half-dozen requests we make in a session, but we
        /// build it once with consistent timeout + UA so all requests
        /// look the same to the server.
        fn agent() -> ureq::Agent {
            ureq::Agent::config_builder()
                .timeout_global(Some(HTTP_TIMEOUT))
                .build()
                .into()
        }

        fn fetch_json(url: &str) -> Result<serde_json::Value, Error> {
            let mut resp = agent()
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", user_agent())
                .call()
                .map_err(|e| Error::Network(e.to_string()))?;
            resp.body_mut()
                .read_json::<serde_json::Value>()
                .map_err(|e| Error::Parse(e.to_string()))
        }

        /// Stream `url` into `dest`, going via a `<dest>.partial` so a
        /// dropped connection never leaves a half-formed file at the
        /// final name. Renames atomically on success.
        fn download_to(url: &str, dest: &Path) -> Result<(), Error> {
            let partial = dest.with_extension(
                dest.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!("{e}.partial"))
                    .unwrap_or_else(|| "partial".to_string()),
            );
            // Cleanup-on-drop guard for the partial file.
            struct PartialGuard(PathBuf);
            impl Drop for PartialGuard {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let guard = PartialGuard(partial.clone());

            let mut resp = agent()
                .get(url)
                .header("User-Agent", user_agent())
                .call()
                .map_err(|e| Error::Network(e.to_string()))?;
            let mut reader = resp.body_mut().as_reader();
            let mut file = std::fs::File::create(&partial)
                .map_err(|e| Error::Io(format!("create {}: {e}", partial.display())))?;
            std::io::copy(&mut reader, &mut file)
                .map_err(|e| Error::Network(format!("body read: {e}")))?;
            file.flush()
                .map_err(|e| Error::Io(format!("flush {}: {e}", partial.display())))?;
            std::fs::rename(&partial, dest)
                .map_err(|e| Error::Io(format!("rename to {}: {e}", dest.display())))?;
            // We renamed to `dest`, so the partial no longer exists.
            // Disarm the guard.
            std::mem::forget(guard);
            Ok(())
        }

        // ─── Parsing the GitHub release ──────────────────────────────

        fn canonical_url(tag: &str, filename: &str) -> String {
            format!(
                "https://github.com/{REPO}/releases/download/{tag}/{filename}",
                REPO = REPO,
            )
        }

        /// Cheap defense against a tampered API response — only trust
        /// `browser_download_url` values that match the canonical
        /// `releases/download/<tag>/<file>` shape on the expected repo.
        fn is_canonical(url: &str, tag: &str, filename: &str) -> bool {
            url == canonical_url(tag, filename)
        }

        /// Parse a `releases/latest` JSON payload into a `Release`,
        /// looking for the asset that matches our arch and pulling its
        /// SHA-256 from the `digest` field GitHub computes for every
        /// release asset.
        fn parse_release(json: &serde_json::Value) -> Result<Release, Error> {
            let tag = json["tag_name"]
                .as_str()
                .ok_or_else(|| Error::Parse("missing tag_name".into()))?;
            let version_str = tag.strip_prefix('v').unwrap_or(tag);
            let version =
                Version::parse(version_str).map_err(|e| Error::Parse(format!("tag {tag}: {e}")))?;
            let arch = current_arch_label().ok_or(Error::UnsupportedEnvironment)?;
            let asset_name = format!("Konstantin-{version}-{arch}.zip");

            let assets = json["assets"]
                .as_array()
                .ok_or_else(|| Error::Parse("missing assets array".into()))?;

            for a in assets {
                let name = a["name"].as_str().unwrap_or("");
                if name != asset_name {
                    continue;
                }
                let url = a["browser_download_url"].as_str().unwrap_or("");
                if !is_canonical(url, tag, &asset_name) {
                    return Err(Error::Parse(format!(
                        "non-canonical asset URL for {asset_name}: {url}"
                    )));
                }
                let digest = a["digest"]
                    .as_str()
                    .ok_or_else(|| Error::Parse(format!("missing digest for {asset_name}")))?;
                let asset_sha256 = parse_digest(digest)?;
                return Ok(Release {
                    version,
                    asset_url: url.to_string(),
                    asset_name,
                    asset_sha256,
                });
            }

            Err(Error::NoAssetForArch)
        }

        /// Parse GitHub's `digest` field: `sha256:<64 hex chars>`.
        /// Returns the lowercase hex without the prefix.
        fn parse_digest(s: &str) -> Result<String, Error> {
            let hex = s
                .strip_prefix("sha256:")
                .ok_or_else(|| Error::Parse(format!("digest is not sha256-prefixed: {s}")))?;
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Error::Parse(format!("digest is not 64 hex chars: {hex}")));
            }
            Ok(hex.to_ascii_lowercase())
        }

        /// Fetch the latest release and decide whether it's newer than
        /// the running build.
        pub fn fetch_latest() -> Result<CheckOutcome, Error> {
            let json = fetch_json(RELEASES_API)?;
            let release = parse_release(&json)?;
            Ok(if release.version > *current_version() {
                CheckOutcome::Newer(release)
            } else {
                CheckOutcome::UpToDate
            })
        }

        // ─── Verification + extraction ───────────────────────────────

        fn sha256_of_file(path: &Path) -> Result<String, Error> {
            let mut hasher = Sha256::new();
            let mut file = std::fs::File::open(path)
                .map_err(|e| Error::Io(format!("open {}: {e}", path.display())))?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| Error::Io(format!("read {}: {e}", path.display())))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let digest = hasher.finalize();
            Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
        }

        fn classify_failure(code: i32, message: &str) -> (&'static str, String) {
            match code {
                10 | 11 => (
                    "Update Failed",
                    format!(
                        "Could not install the update. The previous version is still active.\n\n\
                         Details: {message}"
                    ),
                ),
                20 | 21 | 22 | 23 => (
                    "Update Failed",
                    format!(
                        "The new version did not start; rolled back to the previous version.\n\n\
                         Details: {message}"
                    ),
                ),
                50 => (
                    "Update Failed",
                    "Konstantin's bundle is missing — the rollback did not complete. \
                     Reinstall via Homebrew (`brew reinstall --cask konstantin`) or \
                     download the latest release from GitHub."
                        .to_string(),
                ),
                _ => (
                    "Update Failed",
                    format!("Unexpected error during install.\n\nDetails: {message}"),
                ),
            }
        }

        fn poll_update_result(path: &Path, timeout: Duration) -> Result<UpdateInstallResult> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match std::fs::read_to_string(path) {
                    Ok(json) => {
                        return serde_json::from_str(&json)
                            .map_err(|e| anyhow::anyhow!("parsing update result: {e}"));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "reading update result {}: {e}",
                            path.display()
                        ));
                    }
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for update result");
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        // ─── Top-level flows ─────────────────────────────────────────

        /// Click handler for "Check for Updates…". Runs the GitHub
        /// fetch on a worker thread under a progress panel; on result,
        /// shows the appropriate alert and (on `Newer`) either kicks
        /// off the install or stashes the release for later via
        /// `morphed_for_later`.
        ///
        /// `morphed_for_later` is invoked on the main thread with the
        /// release if the user clicked "Later" — that's where the
        /// caller installs the morphed-menu-item title.
        pub fn run_check_for_updates_flow(
            mtm: MainThreadMarker,
            paths: &bundle::Paths,
            morphed_for_later: impl FnOnce(Release),
        ) {
            // Refuse to update from a dev tree or an unsupported arch.
            if matches!(paths.source, bundle::Source::DevTree) {
                alerts::message(
                    mtm,
                    "Updates Disabled",
                    "Konstantin is running from a developer build. \
                     Use `cargo build` and `packaging/build-app.sh` \
                     to produce a fresh bundle.",
                );
                return;
            }
            if current_arch_label().is_none() {
                alerts::message(
                    mtm,
                    "Updates Disabled",
                    "This Mac's architecture isn't supported by the current release pipeline.",
                );
                return;
            }

            let result = progress::run_with_panel(
                mtm,
                "Checking for Updates",
                "Looking for a newer version of Konstantin…",
                "konstantin-tray-update-check",
                fetch_latest,
            );

            match result {
                Ok(CheckOutcome::UpToDate) => {
                    alerts::message(
                        mtm,
                        "You're Up to Date",
                        &format!("Konstantin {} is the latest.", current_version()),
                    );
                }
                Ok(CheckOutcome::Newer(release)) => {
                    let proceed = alerts::confirm(
                        mtm,
                        "Update Available",
                        &format!("Konstantin {} is available.", release.version),
                        &format!("Update to {}", release.version),
                        "Later",
                    );
                    if proceed {
                        run_install_flow(mtm, paths, &release);
                    } else {
                        morphed_for_later(release);
                    }
                }
                Err(e) => {
                    alerts::message(mtm, "Couldn't Check for Updates", &format!("{e}"));
                }
            }
        }

        /// Download → verify → daemon-mediated install → auto-relaunch.
        /// The root daemon validates and stages the zip, then spawns the
        /// detached updater helper that owns the bundle swap and rollback.
        pub fn run_install_flow(mtm: MainThreadMarker, paths: &bundle::Paths, release: &Release) {
            if paths.bundle_root.is_none() {
                alerts::message(
                    mtm,
                    "Updates Disabled",
                    "Couldn't determine the install location.",
                );
                return;
            }

            // 1. Working dir + RAII cleanup.
            let work_dir =
                std::env::temp_dir().join(format!("konstantin-update-{}", std::process::id()));
            let _wd_guard = WorkDirGuard(work_dir.clone());
            if let Err(e) = std::fs::create_dir_all(&work_dir) {
                alerts::message(
                    mtm,
                    "Update Failed",
                    &format!("Couldn't create temporary directory: {e}"),
                );
                return;
            }

            let zip_path = work_dir.join(&release.asset_name);
            let asset_url = release.asset_url.clone();
            let zp = zip_path.clone();

            // 2. Download under a progress panel.
            let download_result = progress::run_with_panel(
                mtm,
                "Downloading Update",
                &format!("Fetching Konstantin {}…", release.version),
                "konstantin-tray-update-download",
                move || -> Result<(), Error> { download_to(&asset_url, &zp) },
            );
            if let Err(e) = download_result {
                alerts::message(mtm, "Download Failed", &format!("{e}"));
                return;
            }

            // 3. Verify SHA-256 against the digest GitHub returned in
            // the API response (same hash shown on the release page).
            let actual = match sha256_of_file(&zip_path) {
                Ok(h) => h,
                Err(e) => {
                    alerts::message(mtm, "Update Failed", &format!("{e}"));
                    return;
                }
            };
            if actual != release.asset_sha256 {
                tracing::warn!(
                    expected = %release.asset_sha256,
                    actual = %actual,
                    "sha256 mismatch"
                );
                alerts::message(
                    mtm,
                    "Update Failed",
                    "Checksum mismatch — refusing to install.",
                );
                return;
            }

            // 4. Ask the root daemon to validate, stage, and launch
            // the detached updater helper. Then poll the helper's
            // result file while the progress panel stays open.
            let zip_for_install = zip_path.clone();
            let expected_version = release.version.to_string();
            let expected_sha256 = release.asset_sha256.clone();
            let install_result = progress::run_with_panel(
                mtm,
                "Installing Update",
                &format!("Installing Konstantin {}…", release.version),
                "konstantin-tray-update-install",
                move || -> Result<(PathBuf, UpdateInstallResult)> {
                    let response = AdminClient::send_with_timeout(
                        AdminRequest::InstallUpdate {
                            zip_path: zip_for_install,
                            expected_version,
                            expected_sha256,
                        },
                        Duration::from_secs(120),
                    )?;
                    let (result_path, bundle_root) = match response {
                        AdminResponse::UpdateInstallStarted {
                            result_path,
                            bundle_root,
                        } => (result_path, bundle_root),
                        AdminResponse::Unauthorized { reason } => {
                            anyhow::bail!("Not authorized: {reason}");
                        }
                        AdminResponse::Error { message } => anyhow::bail!("{message}"),
                        other => anyhow::bail!("unexpected daemon response: {other:?}"),
                    };
                    let result = poll_update_result(&result_path, Duration::from_secs(120))?;
                    Ok((bundle_root, result))
                },
            );

            match install_result {
                Ok((bundle_root, UpdateInstallResult::Succeeded)) => {
                    let new_tray = bundle_root.join("Contents/MacOS/konstantin-tray");
                    match std::process::Command::new(&new_tray).spawn() {
                        Ok(_) => {
                            tracing::info!(
                                new = %new_tray.display(),
                                "spawned new tray; terminating self"
                            );
                            // Drop the work dir before terminate so
                            // cleanup actually happens.
                            drop(_wd_guard);
                            NSApplication::sharedApplication(mtm).terminate(None);
                        }
                        Err(e) => {
                            // Install succeeded but we couldn't spawn —
                            // tell the user to relaunch manually.
                            alerts::message(
                                mtm,
                                "Update Installed",
                                &format!(
                                    "The update is installed but the tray couldn't relaunch \
                                     automatically. Please reopen Konstantin from \
                                     Applications.\n\nDetails: {e}"
                                ),
                            );
                        }
                    }
                }
                Ok((_, UpdateInstallResult::Failed { code, message })) => {
                    tracing::error!(code, message = %message, "update helper failed");
                    let (title, body) = classify_failure(code, &message);
                    alerts::message(mtm, title, &body);
                }
                Err(e) => alerts::message(mtm, "Update Failed", &e.to_string()),
            }
        }

        // ─── RAII cleanup ────────────────────────────────────────────

        struct WorkDirGuard(PathBuf);
        impl Drop for WorkDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        // ─── Tests ───────────────────────────────────────────────────

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn arch_mapping() {
                // We can't test all branches at runtime — but we can
                // assert the live host maps to a known label or None.
                let label = current_arch_label();
                assert!(matches!(label, None | Some("arm64") | Some("x86_64")));
            }

            #[test]
            fn canonical_url_shape() {
                let url = canonical_url("v0.1.2", "Konstantin-0.1.2-arm64.zip");
                assert_eq!(
                    url,
                    "https://github.com/gitopolis/konstantin/releases/download/v0.1.2/Konstantin-0.1.2-arm64.zip"
                );
            }

            #[test]
            fn rejects_non_canonical_url() {
                assert!(!is_canonical(
                    "https://evil.example/foo.zip",
                    "v0.1.2",
                    "Konstantin-0.1.2-arm64.zip"
                ));
                assert!(is_canonical(
                    "https://github.com/gitopolis/konstantin/releases/download/v0.1.2/Konstantin-0.1.2-arm64.zip",
                    "v0.1.2",
                    "Konstantin-0.1.2-arm64.zip"
                ));
            }

            #[test]
            fn parses_release_payload() {
                let json = serde_json::json!({
                    "tag_name": "v0.1.2",
                    "assets": [
                        {
                            "name": "Konstantin-0.1.2-arm64.zip",
                            "browser_download_url":
                                "https://github.com/gitopolis/konstantin/releases/download/v0.1.2/Konstantin-0.1.2-arm64.zip",
                            "digest": "sha256:dbf5dc5e283847541da9d2222d40951109c59fd5eab277ab03138b226179b5ad"
                        }
                    ]
                });
                // Test under the assumption we're on arm64 — skip on
                // hosts where we can't get an arch label.
                let Some(arch) = current_arch_label() else {
                    return;
                };
                if arch != "arm64" {
                    return;
                }
                let r = parse_release(&json).unwrap();
                assert_eq!(r.version, Version::new(0, 1, 2));
                assert_eq!(r.asset_name, "Konstantin-0.1.2-arm64.zip");
                assert!(r.asset_url.contains("Konstantin-0.1.2-arm64.zip"));
                assert_eq!(
                    r.asset_sha256,
                    "dbf5dc5e283847541da9d2222d40951109c59fd5eab277ab03138b226179b5ad"
                );
            }

            #[test]
            fn rejects_payload_without_digest() {
                let json = serde_json::json!({
                    "tag_name": "v0.1.2",
                    "assets": [
                        {
                            "name": "Konstantin-0.1.2-arm64.zip",
                            "browser_download_url":
                                "https://github.com/gitopolis/konstantin/releases/download/v0.1.2/Konstantin-0.1.2-arm64.zip"
                        }
                    ]
                });
                let Some(arch) = current_arch_label() else {
                    return;
                };
                if arch != "arm64" {
                    return;
                }
                assert!(matches!(parse_release(&json), Err(Error::Parse(_))));
            }

            #[test]
            fn parses_digest() {
                assert_eq!(
                    parse_digest(
                        "sha256:DBF5DC5E283847541DA9D2222D40951109C59FD5EAB277AB03138B226179B5AD"
                    )
                    .unwrap(),
                    "dbf5dc5e283847541da9d2222d40951109c59fd5eab277ab03138b226179b5ad"
                );
                assert!(parse_digest("md5:abcd").is_err());
                assert!(parse_digest("sha256:tooshort").is_err());
                assert!(parse_digest("").is_err());
            }

            #[test]
            fn classify_picks_right_message() {
                let (t, b) = classify_failure(22, "bootstrap failed");
                assert_eq!(t, "Update Failed");
                assert!(b.contains("rolled back"));
                let (_, b) = classify_failure(10, "move aside failed");
                assert!(b.contains("previous version is still active"));
                let (_, b) = classify_failure(50, "catastrophe");
                assert!(b.contains("Reinstall"));
            }
        }
    }

    /// First-launch install + per-user LaunchAgent management.
    ///
    /// Two responsibilities:
    ///   * **System side** — signed bundles register the bundled
    ///     LaunchDaemon through `SMAppService`. Dev-tree runs and
    ///     migration fallback still use the legacy copy/bootstrap script.
    ///   * **User side** (no auth) — write a per-user LaunchAgent plist
    ///     pointing at this tray binary's absolute path, so launchd
    ///     auto-starts us at next login.
    mod install {
        use super::*;
        use std::path::{Path, PathBuf};

        /// macOS LaunchDaemon plist destination (system).
        const SYSTEM_PLIST: &str = "/Library/LaunchDaemons/com.gitopolis.screentimed.plist";
        /// IPC socket — used purely as a liveness probe.
        const SOCKET_PATH: &str = "/var/run/screentimed.sock";

        /// Returns true iff the daemon is currently accepting connections.
        /// Cheap; no privileges required. The connection is closed
        /// immediately — we don't speak the protocol.
        pub fn daemon_socket_reachable() -> bool {
            std::os::unix::net::UnixStream::connect(SOCKET_PATH).is_ok()
        }

        /// Returns true iff the system-side LaunchDaemon plist already
        /// exists. If yes, the daemon is "installed" and any "not
        /// reachable" condition is treated as transient — the subscribe
        /// loop will retry silently.
        pub fn system_plist_present() -> bool {
            Path::new(SYSTEM_PLIST).exists()
        }

        /// Show a Set-up dialog. If the user proceeds, run the
        /// appropriate daemon registration path. Returns
        /// `true` on success, `false` on cancel / failure (alerts the
        /// user before returning false).
        pub fn run_first_launch_install(mtm: MainThreadMarker) -> bool {
            if !alerts::confirm(
                mtm,
                "Set up Konstantin",
                "Konstantin needs to install its background service. \
                 macOS may ask an administrator to approve it.",
                "Set Up…",
                "Quit",
            ) {
                return false;
            }
            let paths = match bundle::Paths::resolve() {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "could not resolve bundle paths");
                    alerts::message(mtm, "Could not locate bundled resources.", &format!("{e}"));
                    return false;
                }
            };

            if paths.source == bundle::Source::Bundle {
                return run_smappservice_install(mtm, &paths);
            }

            run_legacy_install(mtm, &paths)
        }

        fn run_smappservice_install(mtm: MainThreadMarker, paths: &bundle::Paths) -> bool {
            let result = super::progress::run_with_panel(
                mtm,
                "Setting Up Konstantin",
                "Registering Konstantin's background service…",
                "konstantin-smappservice-register",
                super::service_management::register_daemon,
            );

            match result {
                Ok(super::service_management::Status::Enabled) => {
                    tracing::info!("SMAppService daemon enabled");
                    true
                }
                Ok(super::service_management::Status::RequiresApproval) => {
                    super::service_management::open_login_items_settings();
                    alerts::message(
                        mtm,
                        "Approval Needed",
                        "Enable Konstantin in System Settings, then open Konstantin again.",
                    );
                    false
                }
                Ok(status) => {
                    tracing::warn!(?status, "unexpected SMAppService daemon status");
                    alerts::message(
                        mtm,
                        "Konstantin install needs attention.",
                        &format!("Unexpected ServiceManagement status: {status:?}"),
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(error = %e, "SMAppService install failed; offering legacy fallback");
                    if !alerts::confirm(
                        mtm,
                        "Use Legacy Installer?",
                        &format!(
                            "macOS couldn't register the bundled service with ServiceManagement.\n\n{e}\n\nKonstantin can try the older installer instead. You'll be prompted for an administrator password."
                        ),
                        "Use Legacy Installer",
                        "Quit",
                    ) {
                        return false;
                    }
                    run_legacy_install(mtm, paths)
                }
            }
        }

        fn run_legacy_install(mtm: MainThreadMarker, paths: &bundle::Paths) -> bool {
            if paths.source == bundle::Source::Bundle {
                tracing::info!("using legacy LaunchDaemon installer fallback for bundled app");
            }
            let script = build_legacy_install_script(paths);

            match admin::run_with_progress(
                mtm,
                "Setting Up Konstantin",
                "Installing Konstantin's background service.\n\
                 You may be prompted for your administrator password.",
                &script,
            ) {
                Ok(()) => {
                    tracing::info!("first-launch install complete");
                    true
                }
                Err(admin::Error::Cancelled) => {
                    tracing::info!("user cancelled the password prompt");
                    false
                }
                Err(admin::Error::Failed(msg)) => {
                    tracing::error!(error = %msg, "install command failed");
                    alerts::message(mtm, "Konstantin install failed.", &msg);
                    false
                }
            }
        }

        /// Idempotent. Writes `~/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist`
        /// pointing at `current_exe()`. Skips the write if the existing
        /// content is already correct. Does NOT bootstrap — the tray is
        /// already running, and a bootstrap would race-spawn a sibling
        /// instance.
        pub fn ensure_user_launchagent() -> anyhow::Result<()> {
            let exe = std::env::current_exe()?;
            let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
            let home = PathBuf::from(home);
            let agents_dir = home.join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents_dir)?;
            std::fs::create_dir_all(home.join("Library/Logs"))?;
            let dst = agents_dir.join("com.gitopolis.konstantin-tray.plist");
            let want = build_user_launchagent_plist(&exe, &home);

            if let Ok(have) = std::fs::read_to_string(&dst) {
                if have == want {
                    return Ok(());
                }
            }
            std::fs::write(&dst, want)?;
            tracing::info!(path = %dst.display(), "wrote user LaunchAgent plist");
            Ok(())
        }

        fn build_legacy_install_script(p: &bundle::Paths) -> String {
            // Single bash command via `&&` chains. `install -d` creates
            // missing dirs idempotently. Re-running is safe — `cp`
            // overwrites the daemon binary (handles upgrades), and the
            // config copy is guarded by a `[ -f ... ] ||` so an existing
            // `/etc/screentimed/config.toml` is never trampled.
            //
            // `launchctl bootstrap` is ORed with `true` because it fails
            // if the service is already loaded — kickstart -k afterwards
            // forces a restart either way.
            //
            // When installed from a real `.app` bundle, also drop the
            // bundle's absolute path into `/etc/screentimed/bundle_path`
            // so the daemon's bundle-watcher can self-uninstall if the
            // operator drag-to-Trashes the app. In dev-tree mode
            // (`bundle_root = None`) the marker is removed so the
            // watcher stays disabled.
            let mut s = format!(
                "install -d -m 0755 /usr/local/libexec && \
                 install -d -m 0755 /etc/screentimed && \
                 install -d -m 0700 /var/db/screentimed && \
                 install -m 0755 '{daemon}' /usr/local/libexec/screentimed && \
                 install -m 0644 '{plist}' /Library/LaunchDaemons/com.gitopolis.screentimed.plist && \
                 (/usr/libexec/PlistBuddy -c 'Delete :BundleProgram' /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true) && \
                 (/usr/libexec/PlistBuddy -c 'Delete :ProgramArguments' /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true) && \
                 /usr/libexec/PlistBuddy -c 'Add :ProgramArguments array' /Library/LaunchDaemons/com.gitopolis.screentimed.plist && \
                 /usr/libexec/PlistBuddy -c 'Add :ProgramArguments:0 string /usr/local/libexec/screentimed' /Library/LaunchDaemons/com.gitopolis.screentimed.plist && \
                 ([ -f /etc/screentimed/config.toml ] || install -m 0600 '{config}' /etc/screentimed/config.toml)",
                daemon = p.daemon_binary.display(),
                plist = p.daemon_plist.display(),
                config = p.config_example.display(),
            );
            match &p.bundle_root {
                Some(root) => s.push_str(&format!(
                    " && printf '%s\\n' {root_q} > /etc/screentimed/bundle_path",
                    root_q = super::config_ui::shell_quote(root),
                )),
                None => s.push_str(" && rm -f /etc/screentimed/bundle_path"),
            }
            s.push_str(
                " && (launchctl bootstrap system /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true) && \
                 launchctl enable system/com.gitopolis.screentimed && \
                 launchctl kickstart -k system/com.gitopolis.screentimed",
            );
            s
        }

        pub(super) fn build_user_launchagent_plist(tray_exe: &Path, home: &Path) -> String {
            let exe = xml_escape(&tray_exe.display().to_string());
            let stdout = xml_escape(
                &home
                    .join("Library/Logs/konstantin-tray.out.log")
                    .display()
                    .to_string(),
            );
            let stderr = xml_escape(
                &home
                    .join("Library/Logs/konstantin-tray.err.log")
                    .display()
                    .to_string(),
            );
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
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#
            )
        }

        pub(super) fn xml_escape(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&apos;")
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn legacy_installer_rewrites_bundle_program_plist() {
                let paths = bundle::Paths {
                    daemon_binary: PathBuf::from("/Applications/Konstantin.app/Contents/Resources/screentimed"),
                    daemon_plist: PathBuf::from("/Applications/Konstantin.app/Contents/Library/LaunchDaemons/com.gitopolis.screentimed.plist"),
                    config_example: PathBuf::from("/Applications/Konstantin.app/Contents/Resources/config.example.toml"),
                    source: bundle::Source::Bundle,
                    bundle_root: Some(PathBuf::from("/Applications/Konstantin.app")),
                };

                let script = build_legacy_install_script(&paths);

                assert!(script.contains("Delete :BundleProgram"));
                assert!(script.contains("Add :ProgramArguments array"));
                assert!(script
                    .contains("Add :ProgramArguments:0 string /usr/local/libexec/screentimed"));
            }
        }
    }

    /// Native settings window. Replaces the previous "open the TOML in
    /// a text editor" flow with an AppKit window listing every real
    /// local user, with per-user daily-limit and tray-autostart
    /// controls plus an editable warn-thresholds field at the top.
    ///
    /// Configure Open and Save use the daemon's signed admin XPC control
    /// plane. The root daemon reads/writes `/etc/screentimed/config.toml`,
    /// probes other users' LaunchAgent state, applies tray-autostart
    /// changes, and reloads itself after a successful save.
    ///
    /// Other config keys (`enforcement`, `default_policy`,
    /// `kill_switch_path`, paths, `tick_seconds`) are round-tripped
    /// untouched via `toml::Value` — the daemon picks them up on
    /// kickstart.
    mod config_ui {
        use super::*;
        use konstantin_tray::users::{self, LocalUser, UserPicture};
        use objc2::define_class;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyObject, NSObject};
        use objc2::{msg_send, sel, AnyThread, MainThreadOnly};
        use objc2_app_kit::{
            NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
            NSFont, NSImage, NSImageScaling, NSImageView, NSTextField, NSView, NSWindow,
            NSWindowStyleMask,
        };
        use objc2_foundation::{NSData, NSPoint, NSRect, NSSize};
        use std::cell::RefCell;
        use std::path::{Path, PathBuf};

        thread_local! {
            /// At most one configure window at a time. Re-opening just
            /// fronts the existing window. Replaced wholesale on each
            /// open so stale widget retains drop.
            static UI_HANDLE: RefCell<Option<UiHandle>> = const { RefCell::new(None) };
        }

        struct UiHandle {
            window: Retained<NSWindow>,
            // Keep the controller alive while the window is up.
            _controller: Retained<ConfigController>,
            /// Original parsed TOML. Cloned & mutated on save so all
            /// untouched fields (`enforcement`, `default_policy`, …)
            /// are preserved.
            config_value: toml::Value,
            rows: Vec<Row>,
            thresholds_field: Retained<NSTextField>,
        }

        struct Row {
            user: LocalUser,
            limit_check: Retained<NSButton>,
            minutes_field: Retained<NSTextField>,
            autostart_check: Retained<NSButton>,
            /// Snapshot of `<home>/Library/LaunchAgents/...plist`
            /// existence at window-open time. Used to compute the diff
            /// on save.
            autostart_initial: bool,
        }

        define_class!(
            #[unsafe(super(NSObject))]
            #[thread_kind = MainThreadOnly]
            #[name = "KonstantinConfigController"]
            pub struct ConfigController;

            impl ConfigController {
                #[unsafe(method(toggleLimit:))]
                fn toggle_limit_action(&self, sender: Option<&AnyObject>) {
                    let Some(sender) = sender else { return };
                    let tag: isize = unsafe { msg_send![sender, tag] };
                    let state: isize = unsafe { msg_send![sender, state] };
                    let on = state == NSControlStateValueOn;
                    UI_HANDLE.with(|cell| {
                        if let Some(h) = cell.borrow().as_ref() {
                            if let Some(row) = h.rows.get(tag as usize) {
                                row.minutes_field.setEnabled(on);
                            }
                        }
                    });
                }

                #[unsafe(method(toggleAutostart:))]
                fn toggle_autostart_action(&self, _sender: Option<&AnyObject>) {
                    // Stored state is read straight from the checkbox at
                    // save time; nothing else to update on click.
                }

                #[unsafe(method(saveTapped:))]
                fn save_tapped_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    save_flow(mtm);
                }

                #[unsafe(method(cancelTapped:))]
                fn cancel_tapped_action(&self, _sender: Option<&AnyObject>) {
                    close_and_clear();
                }
            }
        );

        impl ConfigController {
            fn new(mtm: MainThreadMarker) -> Retained<Self> {
                let alloc = Self::alloc(mtm);
                unsafe { msg_send![alloc, init] }
            }
        }

        /// Public entry point. Idempotent: re-clicking while the window
        /// is showing just fronts it.
        pub fn open(mtm: MainThreadMarker) {
            let already_visible = UI_HANDLE.with(|cell| {
                cell.borrow()
                    .as_ref()
                    .map(|h| h.window.isVisible())
                    .unwrap_or(false)
            });
            if already_visible {
                #[allow(deprecated)]
                NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
                UI_HANDLE.with(|cell| {
                    if let Some(h) = cell.borrow().as_ref() {
                        h.window.makeKeyAndOrderFront(None);
                        h.window.orderFrontRegardless();
                    }
                });
                return;
            }

            let users_list = match users::enumerate() {
                Ok(u) => u,
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't list users.", &e.to_string());
                    return;
                }
            };
            if users_list.is_empty() {
                super::alerts::message(mtm, "No local users found.", "Nothing to configure.");
                return;
            }

            let probes: Vec<TrayAutostartProbe> = users_list
                .iter()
                .map(|u| TrayAutostartProbe {
                    username: u.username.clone(),
                    home: u.home.clone(),
                })
                .collect();
            let open_response = super::progress::run_with_panel(
                mtm,
                "Open Configuration",
                "Reading /etc/screentimed/config.toml…",
                "konstantin-config-open",
                move || {
                    AdminClient::send(AdminRequest::GetConfig {
                        autostart_probes: probes,
                    })
                },
            );

            let (config_text, autostart_states) = match open_response {
                Ok(AdminResponse::Config {
                    toml,
                    tray_autostart,
                    ..
                }) => (toml, tray_autostart),
                Ok(AdminResponse::Unauthorized { reason }) => {
                    super::alerts::message(mtm, "Administrator required.", &reason);
                    return;
                }
                Ok(AdminResponse::Error { message }) => {
                    super::alerts::message(mtm, "Couldn't read configuration.", &message);
                    return;
                }
                Ok(other) => {
                    super::alerts::message(
                        mtm,
                        "Couldn't read configuration.",
                        &format!("Unexpected daemon response: {other:?}"),
                    );
                    return;
                }
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't read configuration.", &e.to_string());
                    return;
                }
            };

            let config_value: toml::Value = match toml::from_str(&config_text) {
                Ok(v) => v,
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't parse configuration.", &e.to_string());
                    return;
                }
            };

            let initial_thresholds = current_thresholds(&config_value);
            let user_initials =
                collect_user_settings(&users_list, &config_value, &autostart_states);

            let controller = ConfigController::new(mtm);
            let built = build_window(
                mtm,
                &controller,
                &users_list,
                &user_initials,
                &initial_thresholds,
            );

            let window_clone = built.window.clone();
            UI_HANDLE.with(|cell| {
                *cell.borrow_mut() = Some(UiHandle {
                    window: built.window,
                    _controller: controller,
                    config_value,
                    rows: built.rows,
                    thresholds_field: built.thresholds_field,
                });
            });

            // Accessory apps (`LSUIElement=true`) don't auto-activate.
            // `NSApplication::activate` is *cooperative* on macOS 14+
            // ("the framework does not guarantee that the app will be
            // activated at all" — Apple), so it's not enough to steal
            // focus from a regular app. Use the
            // deprecated-but-functional `activateIgnoringOtherApps:`
            // which is the only reliable way to do so.
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            window_clone.makeKeyAndOrderFront(None);
            window_clone.orderFrontRegardless();
        }

        fn close_and_clear() {
            UI_HANDLE.with(|cell| {
                if let Some(h) = cell.borrow_mut().take() {
                    h.window.close();
                }
            });
        }

        // ─── Initial-state helpers ─────────────────────────────────────

        fn current_thresholds(cfg: &toml::Value) -> String {
            let arr = cfg
                .as_table()
                .and_then(|t| t.get("warn_thresholds_minutes"))
                .and_then(|v| v.as_array());
            match arr {
                Some(a) => a
                    .iter()
                    .filter_map(|v| v.as_integer())
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                // Match the daemon default at config.rs:92.
                None => "15, 5, 1".to_string(),
            }
        }

        struct InitialUserSetting {
            limited: bool,
            minutes: u32,
            autostart: bool,
        }

        fn collect_user_settings(
            users: &[LocalUser],
            cfg: &toml::Value,
            autostart_states: &[TrayAutostartState],
        ) -> Vec<InitialUserSetting> {
            let autostart_by_user: std::collections::HashMap<&str, bool> = autostart_states
                .iter()
                .map(|s| (s.username.as_str(), s.enabled))
                .collect();
            let users_table = cfg
                .as_table()
                .and_then(|t| t.get("users"))
                .and_then(|v| v.as_table());
            users
                .iter()
                .map(|u| {
                    let entry = users_table.and_then(|t| t.get(&u.username));
                    let minutes = entry
                        .and_then(|e| e.as_table())
                        .and_then(|t| t.get("daily_limit_minutes"))
                        .and_then(|v| v.as_integer())
                        .map(|n| n.max(1) as u32)
                        .unwrap_or(60);
                    InitialUserSetting {
                        limited: entry.is_some(),
                        minutes,
                        autostart: autostart_by_user
                            .get(u.username.as_str())
                            .copied()
                            .unwrap_or(false),
                    }
                })
                .collect()
        }

        // ─── Window construction ──────────────────────────────────────

        struct Built {
            window: Retained<NSWindow>,
            rows: Vec<Row>,
            thresholds_field: Retained<NSTextField>,
        }

        const WINDOW_WIDTH: f64 = 580.0;
        const SIDE_MARGIN: f64 = 20.0;
        const ROW_HEIGHT: f64 = 44.0;
        const ROW_GAP: f64 = 4.0;
        const AVATAR_SIZE: f64 = 32.0;
        const FIELD_HEIGHT: f64 = 22.0;
        const BUTTON_HEIGHT: f64 = 32.0;

        fn build_window(
            mtm: MainThreadMarker,
            controller: &ConfigController,
            users: &[LocalUser],
            initials: &[InitialUserSetting],
            initial_thresholds: &str,
        ) -> Built {
            // Compute total height so we can place items top-down in
            // bottom-up Cocoa coordinates.
            let title_block = 30.0;
            let thresholds_block = 28.0;
            let users_header_block = 24.0;
            let rows_block = ((ROW_HEIGHT + ROW_GAP) * users.len() as f64 - ROW_GAP).max(0.0);
            let buttons_block = BUTTON_HEIGHT;
            let inner = title_block
                + 12.0
                + thresholds_block
                + 16.0
                + users_header_block
                + rows_block
                + 24.0
                + buttons_block;
            let height = (inner + 2.0 * SIDE_MARGIN).max(280.0);

            let content_rect =
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_WIDTH, height));
            let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            let backing = NSBackingStoreType::Buffered;
            let window: Retained<NSWindow> = unsafe {
                let alloc = NSWindow::alloc(mtm);
                msg_send![
                    alloc,
                    initWithContentRect: content_rect,
                    styleMask: style.0,
                    backing: backing.0,
                    defer: false,
                ]
            };
            window.setTitle(&NSString::from_str("Konstantin Settings"));
            unsafe { window.setReleasedWhenClosed(false) };
            window.center();

            let content = window.contentView().expect("window content view");

            // y starts at the top edge and grows downward as we add
            // items; per-item frame y = height - y_top - item_h.
            let mut y_top = SIDE_MARGIN;

            // Title.
            let title_y = height - y_top - title_block;
            let title = make_title_label(mtm, "Konstantin Settings");
            title.setFrame(NSRect::new(
                NSPoint::new(SIDE_MARGIN, title_y),
                NSSize::new(WINDOW_WIDTH - 2.0 * SIDE_MARGIN, title_block),
            ));
            content.addSubview(&title);
            y_top += title_block + 12.0;

            // Thresholds row: label on the left, text field on the right.
            let thresholds_y = height - y_top - thresholds_block;
            let label = make_label(mtm, "Warn before limit (minutes, comma-separated):");
            label.setFrame(NSRect::new(
                NSPoint::new(SIDE_MARGIN, thresholds_y + 3.0),
                NSSize::new(340.0, 20.0),
            ));
            content.addSubview(&label);
            let thresholds_field = NSTextField::new(mtm);
            thresholds_field.setStringValue(&NSString::from_str(initial_thresholds));
            thresholds_field.setFrame(NSRect::new(
                NSPoint::new(WINDOW_WIDTH - SIDE_MARGIN - 160.0, thresholds_y + 2.0),
                NSSize::new(160.0, FIELD_HEIGHT),
            ));
            content.addSubview(&thresholds_field);
            y_top += thresholds_block + 16.0;

            // Users header.
            let header_y = height - y_top - users_header_block;
            let users_header = make_section_header(mtm, "Users");
            users_header.setFrame(NSRect::new(
                NSPoint::new(SIDE_MARGIN, header_y + 4.0),
                NSSize::new(WINDOW_WIDTH - 2.0 * SIDE_MARGIN, 20.0),
            ));
            content.addSubview(&users_header);
            y_top += users_header_block;

            // Rows.
            let mut rows = Vec::with_capacity(users.len());
            for (i, (user, init)) in users.iter().zip(initials.iter()).enumerate() {
                let row_y = height - y_top - ROW_HEIGHT;
                let built_row = build_row(mtm, controller, i, user, init, row_y);
                content.addSubview(&built_row.container);
                rows.push(Row {
                    user: user.clone(),
                    limit_check: built_row.limit_check,
                    minutes_field: built_row.minutes_field,
                    autostart_check: built_row.autostart_check,
                    autostart_initial: init.autostart,
                });
                y_top += ROW_HEIGHT + ROW_GAP;
            }

            // Buttons row, right-aligned.
            let buttons_y = SIDE_MARGIN;
            let cancel = make_button(mtm, controller, "Cancel", sel!(cancelTapped:));
            cancel.setFrame(NSRect::new(
                NSPoint::new(WINDOW_WIDTH - SIDE_MARGIN - 200.0, buttons_y),
                NSSize::new(90.0, BUTTON_HEIGHT),
            ));
            content.addSubview(&cancel);
            let save = make_button(mtm, controller, "Save", sel!(saveTapped:));
            save.setFrame(NSRect::new(
                NSPoint::new(WINDOW_WIDTH - SIDE_MARGIN - 100.0, buttons_y),
                NSSize::new(100.0, BUTTON_HEIGHT),
            ));
            save.setKeyEquivalent(&NSString::from_str("\r"));
            content.addSubview(&save);

            Built {
                window,
                rows,
                thresholds_field,
            }
        }

        struct BuiltRow {
            container: Retained<NSView>,
            limit_check: Retained<NSButton>,
            minutes_field: Retained<NSTextField>,
            autostart_check: Retained<NSButton>,
        }

        fn build_row(
            mtm: MainThreadMarker,
            controller: &ConfigController,
            index: usize,
            user: &LocalUser,
            init: &InitialUserSetting,
            row_y: f64,
        ) -> BuiltRow {
            let row_rect = NSRect::new(
                NSPoint::new(SIDE_MARGIN, row_y),
                NSSize::new(WINDOW_WIDTH - 2.0 * SIDE_MARGIN, ROW_HEIGHT),
            );
            let container: Retained<NSView> = unsafe {
                let alloc = NSView::alloc(mtm);
                msg_send![alloc, initWithFrame: row_rect]
            };

            // Avatar.
            let avatar_view = NSImageView::new(mtm);
            avatar_view.setFrame(NSRect::new(
                NSPoint::new(0.0, (ROW_HEIGHT - AVATAR_SIZE) / 2.0),
                NSSize::new(AVATAR_SIZE, AVATAR_SIZE),
            ));
            // ScaleAxesIndependently avoids letterboxing inside the
            // square frame — combined with the circular layer mask
            // below, this gives the System Settings look (the photo
            // fills the disc edge-to-edge instead of leaving slivers
            // of transparent backing).
            avatar_view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
            if let Some(image) = load_avatar(mtm, user) {
                avatar_view.setImage(Some(&image));
            }
            // Clip to a circle via the backing CALayer. NSImageView
            // doesn't ship layer-backed by default, so opt in.
            avatar_view.setWantsLayer(true);
            if let Some(layer) = avatar_view.layer() {
                unsafe {
                    let _: () = msg_send![&*layer, setCornerRadius: AVATAR_SIZE / 2.0];
                    let _: () = msg_send![&*layer, setMasksToBounds: true];
                }
            }
            container.addSubview(&avatar_view);

            // Username + role.
            let name_x = AVATAR_SIZE + 12.0;
            let name_label = make_username_label(mtm, &user.username);
            name_label.setFrame(NSRect::new(
                NSPoint::new(name_x, ROW_HEIGHT / 2.0 + 1.0),
                NSSize::new(150.0, 18.0),
            ));
            container.addSubview(&name_label);
            let role_label = make_role_label(mtm, if user.is_admin { "Admin" } else { "Standard" });
            role_label.setFrame(NSRect::new(
                NSPoint::new(name_x, ROW_HEIGHT / 2.0 - 16.0),
                NSSize::new(150.0, 14.0),
            ));
            container.addSubview(&role_label);

            // Limit checkbox.
            let limit_x = name_x + 160.0;
            let limit_check = make_checkbox(
                mtm,
                controller,
                "Limit daily",
                sel!(toggleLimit:),
                index,
                init.limited,
            );
            limit_check.setFrame(NSRect::new(
                NSPoint::new(limit_x, (ROW_HEIGHT - 22.0) / 2.0),
                NSSize::new(105.0, 22.0),
            ));
            container.addSubview(&limit_check);

            // Minutes field.
            let minutes_field = NSTextField::new(mtm);
            minutes_field.setStringValue(&NSString::from_str(&init.minutes.to_string()));
            minutes_field.setEnabled(init.limited);
            minutes_field.setFrame(NSRect::new(
                NSPoint::new(limit_x + 105.0, (ROW_HEIGHT - FIELD_HEIGHT) / 2.0),
                NSSize::new(54.0, FIELD_HEIGHT),
            ));
            minutes_field.setPlaceholderString(Some(&NSString::from_str("min")));
            container.addSubview(&minutes_field);

            // Autostart checkbox.
            let autostart_check = make_checkbox(
                mtm,
                controller,
                "Start at login",
                sel!(toggleAutostart:),
                index,
                init.autostart,
            );
            autostart_check.setFrame(NSRect::new(
                NSPoint::new(limit_x + 170.0, (ROW_HEIGHT - 22.0) / 2.0),
                NSSize::new(140.0, 22.0),
            ));
            container.addSubview(&autostart_check);

            BuiltRow {
                container,
                limit_check,
                minutes_field,
                autostart_check,
            }
        }

        fn load_avatar(mtm: MainThreadMarker, user: &LocalUser) -> Option<Retained<NSImage>> {
            let from_user = match &user.picture {
                Some(UserPicture::File(path)) => {
                    let s = NSString::from_str(&path.display().to_string());
                    let alloc = NSImage::alloc();
                    let img: Option<Retained<NSImage>> =
                        unsafe { msg_send![alloc, initWithContentsOfFile: &*s] };
                    img
                }
                Some(UserPicture::Jpeg(bytes)) => {
                    let data = NSData::with_bytes(bytes);
                    let alloc = NSImage::alloc();
                    let img: Option<Retained<NSImage>> =
                        unsafe { msg_send![alloc, initWithData: &*data] };
                    img
                }
                None => None,
            };
            let _ = mtm; // marker no longer needed; symbol lookup is class-level
            from_user.or_else(|| {
                let name = NSString::from_str("person.crop.circle.fill");
                NSImage::imageWithSystemSymbolName_accessibilityDescription(&name, None)
            })
        }

        // ─── Widget factories ──────────────────────────────────────────

        fn make_title_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
            let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
            label.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
            label
        }

        fn make_section_header(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
            let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
            label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            label
        }

        fn make_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
            NSTextField::labelWithString(&NSString::from_str(text), mtm)
        }

        fn make_username_label(mtm: MainThreadMarker, name: &str) -> Retained<NSTextField> {
            let label = NSTextField::labelWithString(&NSString::from_str(name), mtm);
            label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            label
        }

        fn make_role_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
            let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
            label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            label.setTextColor(Some(&NSColor::secondaryLabelColor()));
            label
        }

        fn make_checkbox(
            mtm: MainThreadMarker,
            controller: &ConfigController,
            title: &str,
            action: objc2::runtime::Sel,
            tag: usize,
            on: bool,
        ) -> Retained<NSButton> {
            let title_ns = NSString::from_str(title);
            let cb = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    &title_ns,
                    Some(controller),
                    Some(action),
                    mtm,
                )
            };
            cb.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            unsafe {
                let _: () = msg_send![&*cb, setTag: tag as isize];
            }
            cb
        }

        fn make_button(
            mtm: MainThreadMarker,
            controller: &ConfigController,
            title: &str,
            action: objc2::runtime::Sel,
        ) -> Retained<NSButton> {
            let title_ns = NSString::from_str(title);
            unsafe {
                NSButton::buttonWithTitle_target_action(
                    &title_ns,
                    Some(controller),
                    Some(action),
                    mtm,
                )
            }
        }

        // ─── Save flow ─────────────────────────────────────────────────

        #[derive(Clone)]
        struct RowSnapshot {
            username: String,
            uid: u32,
            home: PathBuf,
            limited: bool,
            minutes: u32,
            autostart_target: bool,
            autostart_initial: bool,
        }

        struct Snapshot {
            thresholds: Vec<u32>,
            rows: Vec<RowSnapshot>,
            config_value: toml::Value,
        }

        fn save_flow(mtm: MainThreadMarker) {
            let snapshot = match capture_snapshot() {
                Some(s) => s,
                None => return,
            };

            let new_config_text = match build_new_config_toml(&snapshot) {
                Ok(s) => s,
                Err(msg) => {
                    super::alerts::message(mtm, "Invalid configuration", &msg);
                    return;
                }
            };

            let tray_autostart: Vec<TrayAutostartChange> = snapshot
                .rows
                .iter()
                .filter(|row| row.autostart_target != row.autostart_initial)
                .map(|row| TrayAutostartChange {
                    username: row.username.clone(),
                    uid: row.uid,
                    home: row.home.clone(),
                    enabled: row.autostart_target,
                })
                .collect();
            let tray_exe = tray_exe();

            let outcome = super::progress::run_with_panel(
                mtm,
                "Saving Settings",
                "Saving and reloading Konstantin…",
                "konstantin-config-save",
                move || {
                    AdminClient::send(AdminRequest::SetConfig {
                        toml: new_config_text,
                        tray_exe,
                        tray_autostart,
                    })
                },
            );

            match outcome {
                Ok(AdminResponse::EnforcementState { .. }) | Ok(AdminResponse::Ok) => {
                    close_and_clear()
                }
                Ok(AdminResponse::Unauthorized { reason }) => {
                    super::alerts::message(mtm, "Administrator required.", &reason);
                }
                Ok(AdminResponse::Error { message }) => {
                    super::alerts::message(mtm, "Couldn't save settings.", &message);
                }
                Ok(other) => {
                    super::alerts::message(
                        mtm,
                        "Couldn't save settings.",
                        &format!("Unexpected daemon response: {other:?}"),
                    );
                }
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't save settings.", &e.to_string());
                }
            }
        }

        fn capture_snapshot() -> Option<Snapshot> {
            UI_HANDLE.with(|cell| {
                let h_ref = cell.borrow();
                let h = h_ref.as_ref()?;
                let thresholds_text = h.thresholds_field.stringValue().to_string();
                let rows = h
                    .rows
                    .iter()
                    .map(|r| {
                        let limited = r.limit_check.state() == NSControlStateValueOn;
                        let autostart_target = r.autostart_check.state() == NSControlStateValueOn;
                        let minutes = r
                            .minutes_field
                            .stringValue()
                            .to_string()
                            .trim()
                            .parse::<u32>()
                            .unwrap_or(0);
                        RowSnapshot {
                            username: r.user.username.clone(),
                            uid: r.user.uid,
                            home: r.user.home.clone(),
                            limited,
                            minutes,
                            autostart_target,
                            autostart_initial: r.autostart_initial,
                        }
                    })
                    .collect();
                let thresholds = parse_thresholds(&thresholds_text).unwrap_or_default();
                Some(Snapshot {
                    thresholds,
                    rows,
                    config_value: h.config_value.clone(),
                })
            })
        }

        fn parse_thresholds(text: &str) -> Result<Vec<u32>, String> {
            let mut out = Vec::new();
            for tok in text.split(|c: char| c == ',' || c.is_whitespace()) {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                let n: u32 = tok
                    .parse()
                    .map_err(|_| format!("'{tok}' is not a non-negative whole number"))?;
                out.push(n);
            }
            Ok(out)
        }

        fn build_new_config_toml(snap: &Snapshot) -> Result<String, String> {
            for row in &snap.rows {
                if row.limited {
                    if row.minutes == 0 {
                        return Err(format!(
                            "{}: minutes must be greater than 0 when 'Limit daily' is on",
                            row.username
                        ));
                    }
                    if row.minutes > 1440 {
                        return Err(format!(
                            "{}: minutes must be at most 1440 (24 hours)",
                            row.username
                        ));
                    }
                }
            }

            // Round-trip through toml::Value: keeps every other key
            // (enforcement, default_policy, kill_switch_path, paths,
            // tick_seconds) untouched.
            let mut value = snap.config_value.clone();
            let table = value
                .as_table_mut()
                .ok_or_else(|| "config root is not a table".to_string())?;

            let arr: Vec<toml::Value> = snap
                .thresholds
                .iter()
                .map(|n| toml::Value::Integer(*n as i64))
                .collect();
            table.insert(
                "warn_thresholds_minutes".to_string(),
                toml::Value::Array(arr),
            );

            let mut users_table = toml::value::Table::new();
            for row in &snap.rows {
                if !row.limited {
                    continue;
                }
                let mut entry = toml::value::Table::new();
                entry.insert(
                    "daily_limit_minutes".to_string(),
                    toml::Value::Integer(row.minutes as i64),
                );
                users_table.insert(row.username.clone(), toml::Value::Table(entry));
            }
            table.insert("users".to_string(), toml::Value::Table(users_table));

            toml::to_string_pretty(&value).map_err(|e| format!("serializing TOML: {e}"))
        }

        fn tray_exe() -> PathBuf {
            std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/konstantin-tray"))
        }

        pub(super) fn shell_quote(p: &Path) -> String {
            shell_quote_arg(&p.display().to_string())
        }

        pub(super) fn shell_quote_arg(s: &str) -> String {
            // Wrap in single quotes, escape any embedded single quote as
            // `'\''` (close, escape, reopen).
            format!("'{}'", s.replace('\'', "'\\''"))
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn thresholds_parses_comma_separated() {
                assert_eq!(parse_thresholds("15, 5, 1").unwrap(), vec![15, 5, 1]);
                assert_eq!(parse_thresholds("").unwrap(), Vec::<u32>::new());
                assert_eq!(parse_thresholds("  10 ,  2  ").unwrap(), vec![10, 2]);
            }

            #[test]
            fn thresholds_rejects_garbage() {
                assert!(parse_thresholds("15, abc, 1").is_err());
                assert!(parse_thresholds("-3").is_err());
            }

            #[test]
            fn shell_quote_escapes_apostrophe() {
                assert_eq!(shell_quote_arg("alice"), "'alice'");
                assert_eq!(shell_quote_arg("o'brien"), "'o'\\''brien'");
            }

            #[test]
            fn build_config_keeps_unrelated_keys() {
                let original = r#"
enforcement = "logout"
default_policy = "block"
kill_switch_path = "/etc/screentimed/disable"
warn_thresholds_minutes = [30, 10]

[users.alice]
daily_limit_minutes = 30
"#;
                let cfg: toml::Value = toml::from_str(original).unwrap();
                let snap = Snapshot {
                    thresholds: vec![15, 5, 1],
                    rows: vec![RowSnapshot {
                        username: "bob".to_string(),
                        uid: 502,
                        home: PathBuf::from("/Users/bob"),
                        limited: true,
                        minutes: 90,
                        autostart_target: false,
                        autostart_initial: false,
                    }],
                    config_value: cfg,
                };
                let out = build_new_config_toml(&snap).unwrap();
                let parsed: toml::Value = toml::from_str(&out).unwrap();
                let table = parsed.as_table().unwrap();
                assert_eq!(table["enforcement"].as_str(), Some("logout"));
                assert_eq!(table["default_policy"].as_str(), Some("block"));
                let thresholds: Vec<i64> = table["warn_thresholds_minutes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_integer().unwrap())
                    .collect();
                assert_eq!(thresholds, vec![15, 5, 1]);
                let users = table["users"].as_table().unwrap();
                assert!(users.contains_key("bob"));
                // alice was dropped because she wasn't in the snapshot.
                assert!(!users.contains_key("alice"));
                assert_eq!(users["bob"]["daily_limit_minutes"].as_integer(), Some(90));
            }

            #[test]
            fn build_config_validates_minutes() {
                let cfg = toml::Value::Table(toml::value::Table::new());
                let snap = Snapshot {
                    thresholds: vec![],
                    rows: vec![RowSnapshot {
                        username: "alice".into(),
                        uid: 501,
                        home: PathBuf::from("/Users/alice"),
                        limited: true,
                        minutes: 0,
                        autostart_target: false,
                        autostart_initial: false,
                    }],
                    config_value: cfg,
                };
                assert!(build_new_config_toml(&snap).is_err());
            }

            #[test]
            fn launchagent_plist_uses_user_log_paths() {
                let body = super::super::install::build_user_launchagent_plist(
                    Path::new("/Applications/Konstantin.app/Contents/MacOS/konstantin-tray"),
                    Path::new("/Users/alice & bob"),
                );

                assert!(
                    body.contains("/Users/alice &amp; bob/Library/Logs/konstantin-tray.out.log")
                );
                assert!(
                    body.contains("/Users/alice &amp; bob/Library/Logs/konstantin-tray.err.log")
                );
                assert!(!body.contains("/tmp/konstantin-tray"));
            }
        }
    }
}
