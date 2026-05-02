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
    use objc2_app_kit::NSApplicationActivationPolicy;
    use objc2_foundation::{MainThreadMarker, NSString, NSTimer};
    use konstantin_proto::{SessionState, UserStatus};
    use konstantin_tray::notifications::{self, NotifTracker};
    use konstantin_tray::{default_socket_path, format_remaining, Subscription};
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
    struct Latest {
        /// Most recent `UserStatus` from the daemon. `None` until the
        /// first frame arrives. Drained by the timer (it `take`s).
        pending: Option<UserStatus>,
        /// True iff we don't currently have an open subscription. We
        /// start in this state and the worker flips it to `false` once
        /// it actually reaches the daemon.
        disconnected: bool,
    }

    impl Default for Latest {
        fn default() -> Self {
            // Start "disconnected" so the UI shows the muted clock
            // honestly until the worker confirms it can reach the daemon.
            Self {
                pending: None,
                disconnected: true,
            }
        }
    }

    /// All the long-lived AppKit handles the drain timer needs to
    /// touch each tick. Built once by `build_status_item`, moved into
    /// the timer block which the run loop retains for the app's
    /// lifetime.
    struct Tray {
        status_item: Retained<NSStatusItem>,
        start_item: Retained<NSMenuItem>,
        stop_item: Retained<NSMenuItem>,
        restart_item: Retained<NSMenuItem>,
    }

    pub fn main() -> Result<()> {
        install_tracing();

        // Log path-resolution mode early. Useful when a bug report
        // mentions install paths — at a glance you know whether the
        // user is running the production .app bundle or somebody's
        // dev tree.
        match bundle::Paths::resolve() {
            Ok(p) => tracing::info!(
                source = p.source.label(),
                daemon = %p.daemon_binary.display(),
                "konstantin-tray starting"
            ),
            Err(e) => tracing::warn!(error = %e, "could not resolve bundle paths"),
        }

        let mtm = MainThreadMarker::new()
            .expect("konstantin-tray must be launched on the main thread");

        let app = NSApplication::sharedApplication(mtm);
        // Accessory: menu-bar item only — no Dock icon, no main menu.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Controller owns the target/action handlers for menu items.
        // Bound here so it lives until `app.run()` returns (process
        // exit). Menu items hold a weak reference per Cocoa convention.
        let controller = actions::Controller::new(mtm);
        let tray = build_status_item(mtm, &controller);
        let latest = Arc::new(Mutex::new(Latest::default()));

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

        let start_item = make_action_item(mtm, "Start Daemon", sel!(startDaemon:), controller);
        let stop_item = make_action_item(mtm, "Stop Daemon", sel!(stopDaemon:), controller);
        let restart_item =
            make_action_item(mtm, "Restart Daemon", sel!(restartDaemon:), controller);

        // Initial enable-state matches the default `disconnected: true`
        // — only Start is actionable until the worker reports back.
        start_item.setEnabled(true);
        stop_item.setEnabled(false);
        restart_item.setEnabled(false);

        menu.addItem(&start_item);
        menu.addItem(&stop_item);
        menu.addItem(&restart_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let configure = make_action_item(mtm, "Configure…", sel!(configure:), controller);
        let log = make_action_item(mtm, "Open Log", sel!(openLog:), controller);
        // These two are always actionable — no daemon-state dependency.
        configure.setEnabled(true);
        log.setEnabled(true);
        menu.addItem(&configure);
        menu.addItem(&log);

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
            start_item,
            stop_item,
            restart_item,
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

    fn install_drain_timer(tray: Tray, latest: Arc<Mutex<Latest>>) {
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            // Block fires on the main thread (run loop where the timer was
            // scheduled), so we can re-derive the marker safely.
            let mtm = MainThreadMarker::new()
                .expect("drain timer must fire on the main thread");
            let (pending, disconnected) = {
                let mut g = latest.lock().expect("latest mutex");
                (g.pending.take(), g.disconnected)
            };

            // Menu enable-state. Idempotent — `setEnabled` with the
            // current value is a no-op in AppKit, so calling every
            // tick is fine.
            tray.start_item.setEnabled(disconnected);
            tray.stop_item.setEnabled(!disconnected);
            tray.restart_item.setEnabled(!disconnected);

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
    fn apply_visual(
        item: &NSStatusItem,
        disconnected: bool,
        label: &str,
        mtm: MainThreadMarker,
    ) {
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
        #[derive(Debug, Clone, Copy)]
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
                        anyhow::anyhow!(
                            "can't infer workspace root from {}",
                            exe.display()
                        )
                    })?;

                Ok(Self {
                    daemon_binary: profile_dir.join("screentimed"),
                    daemon_plist: workspace
                        .join("packaging/com.gitopolis.screentimed.plist"),
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
                #[unsafe(method(startDaemon:))]
                fn start_daemon_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    // Idempotent. `bootstrap` fails ("I/O error: 5") if
                    // already loaded; that's fine, we just need it
                    // loaded somehow. `enable` fails if already
                    // enabled, also fine. `kickstart` (no `-k`) is the
                    // authoritative step — it brings the service up if
                    // it isn't already, no-ops if it is.
                    run_admin(
                        mtm,
                        "Starting Daemon",
                        "Starting Konstantin…",
                        "(launchctl bootstrap system /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true) && \
                         (launchctl enable system/com.gitopolis.screentimed || true) && \
                         launchctl kickstart system/com.gitopolis.screentimed",
                        "Couldn't start Konstantin.",
                    );
                }

                #[unsafe(method(stopDaemon:))]
                fn stop_daemon_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    // `|| true` so "Stop" is silent when the service
                    // wasn't loaded to begin with. The user's intent
                    // ("not running") is already satisfied.
                    run_admin(
                        mtm,
                        "Stopping Daemon",
                        "Stopping Konstantin…",
                        "launchctl bootout system/com.gitopolis.screentimed || true",
                        "Couldn't stop Konstantin.",
                    );
                }

                #[unsafe(method(restartDaemon:))]
                fn restart_daemon_action(&self, _sender: Option<&AnyObject>) {
                    let mtm = MainThreadMarker::from(self);
                    // Cover both "loaded" and "not loaded" entry states.
                    // `bootstrap || true` makes the loaded-already case
                    // silent. `kickstart -k` then restarts unconditionally.
                    run_admin(
                        mtm,
                        "Restarting Daemon",
                        "Restarting Konstantin…",
                        "(launchctl bootstrap system /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true) && \
                         launchctl kickstart -k system/com.gitopolis.screentimed",
                        "Couldn't restart Konstantin.",
                    );
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

        /// Common shape for "run privileged command, alert on failure".
        fn run_admin(
            mtm: MainThreadMarker,
            panel_title: &str,
            panel_message: &str,
            bash_command: &str,
            failure_title: &str,
        ) {
            match admin::run_with_progress(mtm, panel_title, panel_message, bash_command) {
                Ok(()) => {}
                Err(admin::Error::Cancelled) => {}
                Err(admin::Error::Failed(msg)) => {
                    alerts::message(mtm, failure_title, &msg);
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
        ///   2. Run the privileged teardown via `admin::run_with_progress`
        ///      — bootout the daemon, remove its plist + binaries +
        ///      socket, and `rm` every per-user tray LaunchAgent plist
        ///      under `/Users/<name>/Library/LaunchAgents/`. Mirrors
        ///      `packaging/uninstall.sh`.
        ///   3. Tell the user, then terminate.
        ///
        /// We don't `launchctl bootout gui/<operator-uid>/...` for our
        /// own tray — we *are* that agent, and bootout would terminate
        /// us before the success alert renders. Other users' trays do
        /// get bootout'd so they go away immediately rather than at
        /// next login.
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

            let script = build_uninstall_script();

            match admin::run_with_progress(
                mtm,
                "Uninstalling Konstantin",
                "Stopping the background service and removing files…",
                &script,
            ) {
                Ok(()) => {}
                Err(admin::Error::Cancelled) => return,
                Err(admin::Error::Failed(msg)) => {
                    alerts::message(mtm, "Couldn't uninstall Konstantin.", &msg);
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

        /// Build the `osascript`-driven sudo script that tears down the
        /// system install plus every per-user tray LaunchAgent plist.
        ///
        /// `;` separators (rather than `&&`) so a missing file in one
        /// `rm` doesn't short-circuit the rest. `launchctl bootout` is
        /// suffixed with `|| true` because it errors when the target
        /// isn't loaded — also fine.
        fn build_uninstall_script() -> String {
            let mut parts: Vec<String> = vec![
                "launchctl bootout system/com.gitopolis.screentimed 2>/dev/null || true".into(),
                "rm -f /Library/LaunchDaemons/com.gitopolis.screentimed.plist".into(),
                // Legacy system-level location from pre-phase-7 installs.
                "rm -f /Library/LaunchAgents/com.gitopolis.konstantin-tray.plist".into(),
                "rm -f /usr/local/libexec/screentimed".into(),
                "rm -f /usr/local/bin/konstantin-status".into(),
                "rm -f /usr/local/bin/konstantin-tray".into(),
                "rm -f /var/run/screentimed.sock".into(),
                // Bundle-watcher marker. Always removed so a reinstall
                // from a different location starts clean.
                "rm -f /etc/screentimed/bundle_path".into(),
                // Counter state. A reinstall starts users at zero
                // accumulated time rather than picking up where they
                // left off — matches the user's expectation that
                // uninstalling actually removes their data.
                "rm -rf /var/db/screentimed".into(),
            ];

            // Per-user tray plist cleanup. Iterate every real local
            // account so a multi-user install (operator + others via
            // the Configure UI) is fully cleaned. If enumeration fails,
            // fall back to operator's `$HOME` only.
            let me_uid = super::config_ui::current_uid();
            let users = konstantin_tray::users::enumerate().unwrap_or_default();
            if users.is_empty() {
                if let Ok(home) = std::env::var("HOME") {
                    let plist = std::path::PathBuf::from(home)
                        .join("Library/LaunchAgents/com.gitopolis.konstantin-tray.plist");
                    parts.push(format!(
                        "rm -f {}",
                        super::config_ui::shell_quote(&plist)
                    ));
                }
            } else {
                for u in &users {
                    let plist = u
                        .home
                        .join("Library/LaunchAgents/com.gitopolis.konstantin-tray.plist");
                    parts.push(format!(
                        "rm -f {}",
                        super::config_ui::shell_quote(&plist)
                    ));
                    // Don't bootout our own GUI domain — we *are* that
                    // agent, and bootout would kill us before the
                    // success alert renders. Other users' running trays
                    // can safely be torn down.
                    if u.uid != me_uid {
                        parts.push(format!(
                            "launchctl bootout gui/{uid}/com.gitopolis.konstantin-tray \
                             2>/dev/null || true",
                            uid = u.uid,
                        ));
                    }
                }
            }

            parts.join("; ")
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

    /// Privileged-action primitive: run a bash command as root via
    /// `osascript … with administrator privileges`, with a small
    /// progress panel and a responsive main thread.
    ///
    /// One public function — `run_with_progress`. Used by the install
    /// flow and (in phase A4) by the Start / Stop / Restart / Configure
    /// menu actions. Each call:
    ///   1. Shows a titled NSPanel with an indeterminate spinner and a
    ///      one-liner status message.
    ///   2. Spawns a background thread that invokes osascript (which
    ///      itself shows the OS password sheet, then runs the script as
    ///      root).
    ///   3. Pumps the main run loop in 50 ms slices so the panel
    ///      animates and the cursor stays normal.
    ///   4. Returns when the worker sends its result.
    mod admin {
        use super::*;
        use objc2::rc::Retained;
        use objc2::MainThreadOnly;
        use objc2_app_kit::{
            NSBackingStoreType, NSPanel, NSProgressIndicator, NSProgressIndicatorStyle,
            NSTextField, NSWindowStyleMask,
        };
        use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize};
        use std::sync::mpsc;

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
            let panel = build_progress_panel(mtm, panel_title, panel_message);
            // `orderFrontRegardless` brings the window forward even when
            // the app is `Accessory` (no Dock presence). Combined with
            // `activate`, it puts the panel front-and-centre after the
            // OS-level password prompt dismisses.
            panel.orderFrontRegardless();
            NSApplication::sharedApplication(mtm).activate();

            let cmd = bash_command.to_string();
            let (tx, rx) = mpsc::channel::<Result<(), Error>>();
            std::thread::Builder::new()
                .name("konstantin-tray-admin".into())
                .spawn(move || {
                    let _ = tx.send(run_osascript_blocking(&cmd));
                })
                .expect("spawn admin thread");

            let result = pump_run_loop_until(&rx);

            panel.close();
            result
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
                        unreachable!("admin thread vanished without sending result")
                    }
                }
            }
        }

        fn build_progress_panel(
            mtm: MainThreadMarker,
            title: &str,
            message: &str,
        ) -> Retained<NSPanel> {
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

        fn run_osascript_blocking(bash_command: &str) -> Result<(), Error> {
            // AppleScript double-quoted strings escape `\` and `"`. We
            // build single-line bash here so no newline escaping is
            // needed.
            let escaped = bash_command
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            let applescript = format!(
                r#"do shell script "{escaped}" with administrator privileges"#
            );
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

    /// First-launch install + per-user LaunchAgent management.
    ///
    /// Two responsibilities:
    ///   * **System side** (privileged) — copy the bundled daemon binary
    ///     and plist into `/usr/local/libexec/` and `/Library/LaunchDaemons/`,
    ///     seed a default config, and bootstrap the daemon. Routed
    ///     through `admin::run_with_progress` so the user gets a
    ///     standard password prompt and a progress panel.
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
        /// privileged install via `admin::run_with_progress`. Returns
        /// `true` on success, `false` on cancel / failure (alerts the
        /// user before returning false).
        pub fn run_first_launch_install(mtm: MainThreadMarker) -> bool {
            if !alerts::confirm(
                mtm,
                "Set up Konstantin",
                "Konstantin needs to install its background service. \
                 You'll be prompted for your administrator password.",
                "Set Up…",
                "Quit",
            ) {
                return false;
            }
            let paths = match bundle::Paths::resolve() {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "could not resolve bundle paths");
                    alerts::message(
                        mtm,
                        "Could not locate bundled resources.",
                        &format!("{e}"),
                    );
                    return false;
                }
            };
            let script = build_install_script(&paths);

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
            let home = std::env::var("HOME")
                .map_err(|_| anyhow::anyhow!("HOME not set"))?;
            let agents_dir = PathBuf::from(home).join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents_dir)?;
            let dst = agents_dir.join("com.gitopolis.konstantin-tray.plist");
            let want = build_user_launchagent_plist(&exe);

            if let Ok(have) = std::fs::read_to_string(&dst) {
                if have == want {
                    return Ok(());
                }
            }
            std::fs::write(&dst, want)?;
            tracing::info!(path = %dst.display(), "wrote user LaunchAgent plist");
            Ok(())
        }

        fn build_install_script(p: &bundle::Paths) -> String {
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

        pub(super) fn build_user_launchagent_plist(tray_exe: &Path) -> String {
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

        pub(super) fn xml_escape(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&apos;")
        }
    }

    /// Native settings window. Replaces the previous "open the TOML in
    /// a text editor" flow with an AppKit window listing every real
    /// local user, with per-user daily-limit and tray-autostart
    /// controls plus an editable warn-thresholds field at the top.
    ///
    /// One privileged step on Save covers everything: writing the
    /// updated `/etc/screentimed/config.toml`, installing/removing
    /// LaunchAgents for *other* users, and `launchctl kickstart -k`-ing
    /// the daemon. The operator's *own* tray-autostart flips happen
    /// unprivileged before the admin call.
    ///
    /// Other config keys (`enforcement`, `default_policy`,
    /// `kill_switch_path`, paths, `tick_seconds`) are round-tripped
    /// untouched via `toml::Value` — the daemon picks them up on
    /// kickstart.
    mod config_ui {
        use super::*;
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
        use konstantin_tray::users::{self, LocalUser, UserPicture};
        use std::cell::RefCell;
        use std::path::{Path, PathBuf};

        const SYSTEM_CONFIG: &str = "/etc/screentimed/config.toml";
        const TRAY_AGENT_LABEL: &str = "com.gitopolis.konstantin-tray";
        const TRAY_AGENT_FILENAME: &str = "com.gitopolis.konstantin-tray.plist";

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
            /// Username running this tray instance — drives the
            /// "no admin needed for own user" branch in the autostart
            /// step.
            operator_username: String,
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

            let config_path = Path::new(SYSTEM_CONFIG);
            if !config_path.exists() {
                super::alerts::message(
                    mtm,
                    "No configuration found.",
                    "Set up Konstantin first to create the configuration file.",
                );
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

            // /etc/screentimed/config.toml is 0600 root-owned, so the
            // unprivileged tray can't read it directly. Single admin
            // elevation: copy the file out (chowned to the operator)
            // and, in the same script, dump a manifest of which users
            // currently have a tray LaunchAgent plist on disk —
            // hardened macOS denies an unprivileged tray `stat` access
            // to other users' homes, but root can see them all.
            let operator = current_username();
            let staged_config = tmp_path("konstantin-config-staging", "toml");
            let staged_manifest = tmp_path("konstantin-autostart-staging", "txt");
            let script =
                build_open_admin_script(&operator, &users_list, &staged_config, &staged_manifest);

            match super::admin::run_with_progress(
                mtm,
                "Open Configuration",
                "Reading /etc/screentimed/config.toml…",
                &script,
            ) {
                Ok(()) => {}
                Err(super::admin::Error::Cancelled) => {
                    let _ = std::fs::remove_file(&staged_config);
                    let _ = std::fs::remove_file(&staged_manifest);
                    return;
                }
                Err(super::admin::Error::Failed(msg)) => {
                    super::alerts::message(mtm, "Couldn't read configuration.", &msg);
                    let _ = std::fs::remove_file(&staged_config);
                    let _ = std::fs::remove_file(&staged_manifest);
                    return;
                }
            }

            let config_text = match std::fs::read_to_string(&staged_config) {
                Ok(t) => t,
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't read configuration.", &e.to_string());
                    let _ = std::fs::remove_file(&staged_config);
                    let _ = std::fs::remove_file(&staged_manifest);
                    return;
                }
            };
            let manifest_text = std::fs::read_to_string(&staged_manifest).unwrap_or_default();
            let _ = std::fs::remove_file(&staged_config);
            let _ = std::fs::remove_file(&staged_manifest);

            let config_value: toml::Value = match toml::from_str(&config_text) {
                Ok(v) => v,
                Err(e) => {
                    super::alerts::message(mtm, "Couldn't parse configuration.", &e.to_string());
                    return;
                }
            };

            let manifest = parse_autostart_manifest(&manifest_text);
            let initial_thresholds = current_thresholds(&config_value);
            let user_initials =
                collect_user_settings(&users_list, &config_value, &operator, &manifest);

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
                    operator_username: operator,
                });
            });

            // After `osascript … with administrator privileges`,
            // SecurityAgent dismisses and macOS hands focus back to
            // whichever app was previously frontmost (e.g. VSCode),
            // *not* us — accessory apps (`LSUIElement=true`) don't
            // auto-activate. `NSApplication::activate` is *cooperative*
            // on macOS 14+ ("the framework does not guarantee that the
            // app will be activated at all" — Apple), so it's not
            // enough to steal focus from a regular app. Use the
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
            operator: &str,
            manifest: &std::collections::HashMap<String, bool>,
        ) -> Vec<InitialUserSetting> {
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
                        autostart: autostart_state(u, operator, manifest),
                    }
                })
                .collect()
        }

        /// Initial autostart state for a row.
        ///
        /// For the operator's own user, `stat` is authoritative. For
        /// other users, hardened macOS denies the unprivileged tray
        /// `stat` access to `/Users/<other>/Library/LaunchAgents/` —
        /// so the open-flow admin script (run as root) emits a
        /// manifest of plist presence per user, and we look up the
        /// answer there.
        fn autostart_state(
            user: &LocalUser,
            operator: &str,
            manifest: &std::collections::HashMap<String, bool>,
        ) -> bool {
            if user.username == operator {
                return autostart_present(user);
            }
            manifest.get(&user.username).copied().unwrap_or(false)
        }

        fn autostart_present(user: &LocalUser) -> bool {
            agent_path(user).is_file()
        }

        fn agent_path(user: &LocalUser) -> PathBuf {
            user.home.join("Library/LaunchAgents").join(TRAY_AGENT_FILENAME)
        }

        fn current_username() -> String {
            // `USER` env var works for tray launches via Dock / Finder /
            // launchd, but `getpwuid(getuid())` is the authoritative
            // source. Try the cheap path first.
            if let Ok(name) = std::env::var("USER") {
                if !name.is_empty() {
                    return name;
                }
            }
            std::process::Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        }

        /// Parse the per-user autostart manifest dumped by the
        /// open-flow admin script. Format: one `<username> 0|1` line
        /// per user. Empty / malformed input yields an empty map.
        fn parse_autostart_manifest(text: &str) -> std::collections::HashMap<String, bool> {
            text.lines()
                .filter_map(|line| {
                    let mut it = line.splitn(2, ' ');
                    let user = it.next()?.trim();
                    let flag = it.next()?.trim();
                    if user.is_empty() {
                        return None;
                    }
                    Some((user.to_string(), flag == "1"))
                })
                .collect()
        }

        /// Build the open-flow admin script: copy the 0600 root-owned
        /// config out to a user-owned temp, and dump a manifest of
        /// per-user tray-LaunchAgent plist presence to a sibling temp.
        /// One elevation, both side effects.
        fn build_open_admin_script(
            operator: &str,
            users: &[LocalUser],
            staged_config: &Path,
            staged_manifest: &Path,
        ) -> String {
            let mut parts: Vec<String> = vec![format!(
                "install -m 0600 -o {user} -g staff /etc/screentimed/config.toml {dst}",
                user = shell_quote_arg(operator),
                dst = shell_quote(staged_config),
            )];

            let mut probe = String::from("(");
            for u in users {
                let p = u.home.join("Library/LaunchAgents").join(TRAY_AGENT_FILENAME);
                probe.push_str(&format!(
                    "if test -f {path}; then echo {name} 1; else echo {name} 0; fi; ",
                    path = shell_quote(&p),
                    name = shell_quote_arg(&u.username),
                ));
            }
            probe.push_str(") > ");
            probe.push_str(&shell_quote(staged_manifest));
            parts.push(probe);

            parts.push(format!(
                "chown {user}:staff {dst}",
                user = shell_quote_arg(operator),
                dst = shell_quote(staged_manifest),
            ));
            parts.push(format!(
                "chmod 0600 {dst}",
                dst = shell_quote(staged_manifest),
            ));

            parts.join(" && ")
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
            operator_username: String,
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

            let config_temp = tmp_path("konstantin-config", "toml");
            if let Err(e) = std::fs::write(&config_temp, &new_config_text) {
                super::alerts::message(mtm, "Couldn't write temp config.", &e.to_string());
                return;
            }

            // Apply self-changes unprivileged so the password prompt
            // covers only things that genuinely need root.
            let mut other_user_changes: Vec<(PathBuf, RowSnapshot, bool)> = Vec::new();
            for row in &snapshot.rows {
                if row.autostart_target == row.autostart_initial {
                    continue;
                }
                if row.username == snapshot.operator_username {
                    if row.autostart_target {
                        if let Err(e) = enable_autostart_self(&row.home) {
                            tracing::warn!(error = %e, "self autostart enable failed");
                        }
                    } else if let Err(e) = disable_autostart_self(&row.home) {
                        tracing::warn!(error = %e, "self autostart disable failed");
                    }
                    continue;
                }
                if row.autostart_target {
                    let plist_temp =
                        tmp_path(&format!("konstantin-agent-{}", row.username), "plist");
                    let plist_body =
                        super::install::build_user_launchagent_plist(&tray_exe());
                    if let Err(e) = std::fs::write(&plist_temp, plist_body) {
                        super::alerts::message(
                            mtm,
                            "Couldn't write temp plist.",
                            &e.to_string(),
                        );
                        let _ = std::fs::remove_file(&config_temp);
                        return;
                    }
                    other_user_changes.push((plist_temp, row.clone(), true));
                } else {
                    other_user_changes.push((PathBuf::new(), row.clone(), false));
                }
            }

            let script = build_admin_script(&config_temp, &other_user_changes);

            let outcome = super::admin::run_with_progress(
                mtm,
                "Saving Settings",
                "Saving and reloading Konstantin…",
                &script,
            );

            // Cleanup all temp files regardless of outcome.
            let _ = std::fs::remove_file(&config_temp);
            for (path, _, _) in &other_user_changes {
                if !path.as_os_str().is_empty() {
                    let _ = std::fs::remove_file(path);
                }
            }

            match outcome {
                Ok(()) => close_and_clear(),
                Err(super::admin::Error::Cancelled) => {}
                Err(super::admin::Error::Failed(msg)) => {
                    super::alerts::message(mtm, "Couldn't save settings.", &msg);
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
                        let autostart_target =
                            r.autostart_check.state() == NSControlStateValueOn;
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
                    operator_username: h.operator_username.clone(),
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
            table.insert("warn_thresholds_minutes".to_string(), toml::Value::Array(arr));

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

        fn build_admin_script(
            config_temp: &Path,
            other_user_changes: &[(PathBuf, RowSnapshot, bool)],
        ) -> String {
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!(
                "install -m 0600 {src} /etc/screentimed/config.toml",
                src = shell_quote(config_temp),
            ));
            for (plist_temp, row, enable) in other_user_changes {
                let dest_dir = row.home.join("Library/LaunchAgents");
                let dest_plist = dest_dir.join(TRAY_AGENT_FILENAME);
                if *enable {
                    parts.push(format!(
                        "install -d -o {user} -g staff -m 0755 {dir}",
                        user = shell_quote_arg(&row.username),
                        dir = shell_quote(&dest_dir),
                    ));
                    parts.push(format!(
                        "install -m 0644 -o {user} -g staff {src} {dst}",
                        user = shell_quote_arg(&row.username),
                        src = shell_quote(plist_temp),
                        dst = shell_quote(&dest_plist),
                    ));
                    parts.push(format!(
                        "(launchctl print gui/{uid} >/dev/null 2>&1 && \
                          launchctl bootstrap gui/{uid} {dst} || true)",
                        uid = row.uid,
                        dst = shell_quote(&dest_plist),
                    ));
                } else {
                    parts.push(format!(
                        "(rm -f {dst}; launchctl bootout gui/{uid}/{label} 2>/dev/null || true)",
                        dst = shell_quote(&dest_plist),
                        uid = row.uid,
                        label = TRAY_AGENT_LABEL,
                    ));
                }
            }
            parts.push("launchctl kickstart -k system/com.gitopolis.screentimed".to_string());
            parts.join(" && ")
        }

        fn enable_autostart_self(home: &Path) -> std::io::Result<()> {
            let agents = home.join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents)?;
            let dst = agents.join(TRAY_AGENT_FILENAME);
            let body = super::install::build_user_launchagent_plist(&tray_exe());
            std::fs::write(&dst, body)?;
            // Best-effort bootstrap into our own GUI domain.
            let uid = current_uid();
            let _ = std::process::Command::new("/bin/launchctl")
                .args(["bootstrap", &format!("gui/{uid}")])
                .arg(&dst)
                .status();
            Ok(())
        }

        fn disable_autostart_self(home: &Path) -> std::io::Result<()> {
            let dst = home.join("Library/LaunchAgents").join(TRAY_AGENT_FILENAME);
            let _ = std::fs::remove_file(&dst);
            let uid = current_uid();
            let _ = std::process::Command::new("/bin/launchctl")
                .args(["bootout", &format!("gui/{uid}/{TRAY_AGENT_LABEL}")])
                .status();
            Ok(())
        }

        fn tray_exe() -> PathBuf {
            std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/konstantin-tray"))
        }

        fn tmp_path(stem: &str, ext: &str) -> PathBuf {
            let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp/".to_string());
            PathBuf::from(format!("{dir}{stem}-{}.{ext}", std::process::id()))
        }

        pub(super) fn shell_quote(p: &Path) -> String {
            shell_quote_arg(&p.display().to_string())
        }

        pub(super) fn shell_quote_arg(s: &str) -> String {
            // Wrap in single quotes, escape any embedded single quote as
            // `'\''` (close, escape, reopen).
            format!("'{}'", s.replace('\'', "'\\''"))
        }

        pub(super) fn current_uid() -> u32 {
            extern "C" {
                fn getuid() -> u32;
            }
            unsafe { getuid() }
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
                    operator_username: "nikita".to_string(),
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
                    operator_username: "nikita".into(),
                };
                assert!(build_new_config_toml(&snap).is_err());
            }
        }
    }
}
