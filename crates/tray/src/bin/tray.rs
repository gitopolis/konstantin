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
    use anyhow::{Context, Result};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::sel;
    use objc2_app_kit::{
        NSAlert, NSApplication, NSCellImagePosition, NSColor, NSForegroundColorAttributeName,
        NSImage, NSImageSymbolConfiguration, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
        NSVariableStatusItemLength,
    };
    // `NSApplicationActivationPolicy` is declared in the
    // NSRunningApplication header, not NSApplication's.
    use konstantin_proto::admin::{AdminRequest, AdminResponse};
    use konstantin_proto::{SessionState, UserStatus};
    use konstantin_tray::admin_xpc::AdminClient;
    use konstantin_tray::notifications::{self, NotifTracker};
    use konstantin_tray::{default_socket_path, format_remaining, Subscription};
    use objc2_app_kit::NSApplicationActivationPolicy;
    use objc2_foundation::{MainThreadMarker, NSAttributedString, NSDictionary, NSString, NSTimer};
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
    }

    /// Shared `Arc<Mutex<Latest>>` set during `main()` so action
    /// handlers (which can't be parameterised through the
    /// `define_class!` macro without ivars) can read live state.
    /// `OnceLock` so it's a fail-loud configuration mistake to install
    /// it twice.
    static LATEST: std::sync::OnceLock<Arc<Mutex<Latest>>> = std::sync::OnceLock::new();

    struct InstanceLock {
        _file: std::fs::File,
    }

    fn acquire_instance_lock() -> Result<Option<InstanceLock>> {
        use std::os::fd::AsRawFd;

        let uid = unsafe { libc::geteuid() };
        let path = std::env::temp_dir().join(format!("com.gitopolis.konstantin-tray-{uid}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(InstanceLock { _file: file }));
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(error.into())
        }
    }

    pub fn main() -> Result<()> {
        install_tracing();
        let mut args = std::env::args_os().skip(1);
        match args.next().as_deref() {
            Some(arg) if arg == std::ffi::OsStr::new("--schedule-uninstall-services") => {
                return schedule_uninstall_services();
            }
            Some(arg) if arg == std::ffi::OsStr::new("--deferred-uninstall-services") => {
                let ready = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("missing deferred-uninstall ready path"))?;
                return deferred_uninstall_services(&ready);
            }
            Some(arg) => anyhow::bail!("unknown argument: {}", arg.to_string_lossy()),
            None => {}
        }
        let Some(_instance_lock) = acquire_instance_lock()? else {
            tracing::info!("another Konstantin tray instance is already running; exiting");
            return Ok(());
        };

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
        apply_visual(&tray.status_item, true, StatusDisplay::empty(), mtm);

        let daemon_state = install::daemon_lifecycle_state().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not inspect daemon lifecycle state");
            install::DaemonLifecycleState::NotRegistered
        });
        match daemon_state {
            install::DaemonLifecycleState::NotRegistered
            | install::DaemonLifecycleState::RequiresApproval => {
                if !install::run_first_launch_install(mtm) {
                    tracing::info!("first-launch setup not completed; quitting");
                    return Ok(());
                }
            }
            install::DaemonLifecycleState::EnabledHealthy => {
                install::maybe_refresh_daemon_version(mtm);
            }
            install::DaemonLifecycleState::EnabledMissingSocket => {
                tracing::warn!("managed daemon is enabled but its socket is not ready");
            }
            install::DaemonLifecycleState::EnabledMissingAdminEndpoint => {
                install::maybe_run_admin_control_repair(mtm);
            }
        }

        match bundle::Paths::resolve().map(|p| p.source) {
            Ok(bundle::Source::Bundle) => install::ensure_user_agent(mtm),
            Ok(bundle::Source::DevTree) => {
                tracing::info!("dev-tree run — skipping managed user-agent registration");
            }
            Err(_) => {} // already logged above
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

    fn schedule_uninstall_services() -> Result<()> {
        use std::os::unix::process::CommandExt;

        let ready =
            std::env::temp_dir().join(format!("konstantin-uninstall-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("--deferred-uninstall-services")
            .arg(&ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if ready.is_file() {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("deferred uninstaller exited before it was ready: {status}");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                anyhow::bail!("timed out starting deferred uninstaller");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn deferred_uninstall_services(ready: &std::path::Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let paths = bundle::Paths::resolve()?;
        let bundle_root = paths
            .bundle_root
            .ok_or_else(|| anyhow::anyhow!("deferred uninstall requires an app bundle"))?;
        let original_inode = std::fs::metadata(&bundle_root)?.ino();
        let handles = service_management::unregister_handles()?;
        std::fs::write(ready, b"ready\n")?;

        let removed = wait_for_bundle_removal(&bundle_root, original_inode)?;
        let _ = std::fs::remove_file(ready);
        if !removed {
            tracing::info!("bundle was replaced during upgrade; keeping services registered");
            return Ok(());
        }

        let cleanup = match AdminClient::send_with_timeout(
            AdminRequest::PrepareUninstall {
                preserve_config: true,
            },
            Duration::from_secs(10),
        ) {
            Ok(AdminResponse::Ok) => Ok(()),
            Ok(AdminResponse::Unauthorized { reason }) => Err(anyhow::anyhow!(
                "not authorized to prepare uninstall: {reason}"
            )),
            Ok(AdminResponse::Error { message }) => Err(anyhow::anyhow!(message)),
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected daemon response during uninstall: {other:?}"
            )),
            Err(e) => {
                tracing::warn!(error = %e, "daemon cleanup unavailable during uninstall");
                Ok(())
            }
        };

        handles.unregister()?;
        cleanup
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BundleObservation {
        Original,
        Replaced,
        Missing,
    }

    fn observe_bundle(
        bundle_root: &std::path::Path,
        original_inode: u64,
    ) -> std::io::Result<BundleObservation> {
        use std::os::unix::fs::MetadataExt;

        match std::fs::metadata(bundle_root) {
            Ok(metadata) if metadata.ino() != original_inode => Ok(BundleObservation::Replaced),
            Ok(_) => Ok(BundleObservation::Original),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BundleObservation::Missing),
            Err(e) => Err(e),
        }
    }

    fn wait_for_bundle_removal(bundle_root: &std::path::Path, original_inode: u64) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut missing_since = None;
        loop {
            match observe_bundle(bundle_root, original_inode) {
                Ok(BundleObservation::Replaced) => return Ok(false),
                Ok(BundleObservation::Original) => missing_since = None,
                Ok(BundleObservation::Missing) => {
                    let since = missing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(20) {
                        return Ok(true);
                    }
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("checking app bundle at {}", bundle_root.display())
                    });
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for Homebrew to remove or replace the app bundle");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    #[cfg(test)]
    mod lifecycle_tests {
        use super::*;
        use std::os::unix::fs::MetadataExt;

        #[test]
        fn observes_original_replaced_and_missing_bundle() {
            let base = std::env::temp_dir().join(format!(
                "konstantin-bundle-observation-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let bundle = base.join("Konstantin.app");
            std::fs::create_dir_all(&bundle).unwrap();
            let inode = std::fs::metadata(&bundle).unwrap().ino();
            assert_eq!(
                observe_bundle(&bundle, inode).unwrap(),
                BundleObservation::Original
            );

            std::fs::rename(&bundle, base.join("Old.app")).unwrap();
            std::fs::create_dir(&bundle).unwrap();
            assert_eq!(
                observe_bundle(&bundle, inode).unwrap(),
                BundleObservation::Replaced
            );

            std::fs::remove_dir(&bundle).unwrap();
            assert_eq!(
                observe_bundle(&bundle, inode).unwrap(),
                BundleObservation::Missing
            );
            let _ = std::fs::remove_dir_all(base);
        }
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
            let (pending, disconnected, enforcement_paused) = {
                let mut g = latest.lock().expect("latest mutex");
                (g.pending.take(), g.disconnected, g.enforcement_paused)
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

            // Visuals. The muted clock trumps any pending status if
            // we're currently disconnected — even if a stale `pending`
            // is sitting around, the daemon is unreachable *now*.
            if disconnected {
                apply_visual(&tray.status_item, true, StatusDisplay::empty(), mtm);
            } else if let Some(status) = pending {
                apply_visual(&tray.status_item, false, status_display(&status), mtm);
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StatusDisplay {
        label: String,
        urgent: bool,
    }

    impl StatusDisplay {
        fn empty() -> Self {
            Self {
                label: String::new(),
                urgent: false,
            }
        }
    }

    fn status_display(status: &UserStatus) -> StatusDisplay {
        let (label, urgent) = match status.state {
            // No limit configured for this account — clock glyph alone
            // is enough; an em-dash next to it just looks like noise.
            SessionState::NotConfigured => (String::new(), false),
            SessionState::Offline => ("offline".to_string(), false),
            SessionState::LimitReached => ("0s".to_string(), true),
            SessionState::Active => (
                format_remaining(status.remaining_seconds),
                (0..60).contains(&status.remaining_seconds),
            ),
            SessionState::Paused => (
                format!("⏸ {}", format_remaining(status.remaining_seconds)),
                false,
            ),
        };
        StatusDisplay { label, urgent }
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
        display: StatusDisplay,
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
        let title = NSString::from_str(&display.label);
        if display.urgent {
            let red: Retained<AnyObject> = NSColor::systemRedColor().into();
            // SAFETY: AppKit provides this process-wide NSString constant.
            let foreground_color_attr = unsafe { NSForegroundColorAttributeName };
            let attrs = NSDictionary::from_retained_objects(&[foreground_color_attr], &[red]);
            // SAFETY: The attributes dictionary contains a valid AppKit text
            // foreground-color attribute with an NSColor value.
            let attributed = unsafe { NSAttributedString::new_with_attributes(&title, &attrs) };
            button.setAttributedTitle(&attributed);
        } else {
            button.setTitle(&title);
        }
    }

    fn install_tracing() {
        let filter = EnvFilter::try_from_env("KONSTANTIN_TRAY_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,konstantin_tray=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    }

    #[cfg(test)]
    mod status_display_tests {
        use super::*;
        use chrono::Local;

        fn status(state: SessionState, remaining_seconds: i64) -> UserStatus {
            UserStatus {
                uid: 501,
                username: "alice".to_string(),
                state,
                daily_limit_seconds: 3600,
                used_seconds: 3600u32.saturating_sub(remaining_seconds.max(0) as u32),
                remaining_seconds,
                resets_at: Local::now(),
                warn_thresholds_minutes: vec![],
            }
        }

        #[test]
        fn active_at_sixty_seconds_is_not_urgent() {
            assert_eq!(
                status_display(&status(SessionState::Active, 60)),
                StatusDisplay {
                    label: "1m00s".to_string(),
                    urgent: false,
                }
            );
        }

        #[test]
        fn active_below_sixty_seconds_is_urgent() {
            for remaining in [59, 1, 0] {
                let display = status_display(&status(SessionState::Active, remaining));
                assert_eq!(display.label, format_remaining(remaining));
                assert!(display.urgent);
            }
        }

        #[test]
        fn limit_reached_is_urgent_zero() {
            assert_eq!(
                status_display(&status(SessionState::LimitReached, 0)),
                StatusDisplay {
                    label: "0s".to_string(),
                    urgent: true,
                }
            );
        }

        #[test]
        fn non_counting_states_are_not_urgent() {
            for state in [
                SessionState::Paused,
                SessionState::Offline,
                SessionState::NotConfigured,
            ] {
                assert!(!status_display(&status(state, 30)).urgent);
            }
        }
    }

    /// Resolves paths to the daemon binary, daemon plist template, and
    /// example config — either from this `.app` bundle's
    /// `Contents/Resources/` (production) or from `target/<profile>/`
    /// + `packaging/` (developer running `cargo run` or
    ///   `target/release/konstantin-tray` directly).
    ///
    /// One source of truth so setup and diagnostics resolve the same
    /// bundled artifacts through `bundle::Paths::resolve()`.
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
            pub source: Source,
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
                            source: Source::Bundle,
                            bundle_root,
                        });
                    }
                }

                // Dev-tree fallback. `exe_dir` is `target/<profile>/`
                // (`release` or `debug`); the daemon binary lives next
                // to the tray.
                let profile_dir = exe_dir;

                Ok(Self {
                    daemon_binary: profile_dir.join("screentimed"),
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

        /// Homebrew owns the app bundle and package receipt. Keep the menu
        /// action as discoverable guidance rather than deleting files behind
        /// Homebrew from inside the app.
        fn uninstall_flow(mtm: MainThreadMarker) {
            alerts::message(
                mtm,
                "Uninstall Konstantin with Homebrew",
                "Run `brew uninstall --cask konstantin` in Terminal.\n\nTo also remove configuration, logs, preferences, and counters, use `brew uninstall --cask --zap konstantin`.",
            );
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

    mod service_management {
        use super::*;
        use block2::RcBlock;
        use objc2::msg_send;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyClass, AnyObject};
        use objc2_foundation::NSString;
        use std::ptr;
        use std::sync::mpsc;

        #[link(name = "ServiceManagement", kind = "framework")]
        extern "C" {}

        const DAEMON_PLIST_NAME: &str = "com.gitopolis.screentimed.plist";
        const AGENT_PLIST_NAME: &str = "com.gitopolis.konstantin-tray.plist";
        const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(15);

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Status {
            NotRegistered,
            Enabled,
            RequiresApproval,
            NotFound,
            Unknown(isize),
        }

        pub struct UnregisterHandles {
            agent: Retained<AnyObject>,
            daemon: Retained<AnyObject>,
        }

        impl UnregisterHandles {
            pub fn unregister(self) -> Result<()> {
                unregister_if_present(self.agent)?;
                unregister_if_present(self.daemon)
            }
        }

        pub fn daemon_status() -> Result<Status> {
            let service = daemon_service()?;
            Ok(status_of(&service))
        }

        pub fn agent_status() -> Result<Status> {
            let service = agent_service()?;
            Ok(status_of(&service))
        }

        pub fn register_daemon() -> Result<Status> {
            let service = daemon_service()?;
            register_service(&service, daemon_status)
        }

        pub fn ensure_agent_registered() -> Result<Status> {
            match agent_status()? {
                Status::Enabled => Ok(Status::Enabled),
                Status::RequiresApproval => Ok(Status::RequiresApproval),
                Status::NotRegistered | Status::NotFound | Status::Unknown(_) => {
                    let service = agent_service()?;
                    register_service(&service, agent_status)
                }
            }
        }

        fn register_service(
            service: &AnyObject,
            status: impl Fn() -> Result<Status>,
        ) -> Result<Status> {
            let mut error: *mut AnyObject = ptr::null_mut();
            let ok: bool = unsafe { msg_send![service, registerAndReturnError: &mut error] };
            if !ok {
                let current = status().unwrap_or(Status::NotFound);
                if matches!(current, Status::Enabled | Status::RequiresApproval) {
                    return Ok(current);
                }
                anyhow::bail!("{}", error_message(error));
            }
            status()
        }

        pub fn refresh_daemon_registration() -> Result<Status> {
            unregister_service(daemon_service()?)?;
            register_daemon()
        }

        pub fn unregister_handles() -> Result<UnregisterHandles> {
            Ok(UnregisterHandles {
                agent: agent_service()?,
                daemon: daemon_service()?,
            })
        }

        fn unregister_if_present(service: Retained<AnyObject>) -> Result<()> {
            if matches!(
                status_of(&service),
                Status::NotRegistered | Status::NotFound
            ) {
                return Ok(());
            }
            unregister_service(service)
        }

        fn status_of(service: &AnyObject) -> Status {
            let raw: isize = unsafe { msg_send![service, status] };
            Status::from_raw(raw)
        }

        fn unregister_service(service: Retained<AnyObject>) -> Result<()> {
            let (tx, rx) = mpsc::sync_channel(1);
            let completion = RcBlock::new(move |error: *mut AnyObject| {
                let result = if error.is_null() {
                    Ok(())
                } else {
                    Err(error_message(error))
                };
                let _ = tx.send(result);
            });
            unsafe {
                let _: () = msg_send![&*service, unregisterWithCompletionHandler: &*completion];
            }
            match rx.recv_timeout(UNREGISTER_TIMEOUT) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(message)) => anyhow::bail!("{message}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("timed out waiting for ServiceManagement to unregister")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("ServiceManagement unregister callback disconnected")
                }
            }
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

        fn agent_service() -> Result<Retained<AnyObject>> {
            let cls = sm_app_service_class()
                .ok_or_else(|| anyhow::anyhow!("SMAppService is unavailable"))?;
            let plist = NSString::from_str(AGENT_PLIST_NAME);
            let service: Option<Retained<AnyObject>> =
                unsafe { msg_send![cls, agentServiceWithPlistName: &*plist] };
            service.ok_or_else(|| anyhow::anyhow!("SMAppService returned nil agent service"))
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

    /// First-launch setup and managed per-user agent registration.
    ///
    /// Two responsibilities:
    ///   * **System side** — signed bundles register the bundled
    ///     LaunchDaemon through `SMAppService`.
    ///   * **User side** — register the bundled LaunchAgent through
    ///     `SMAppService.agent(plistName:)` in the current login session.
    mod install {
        use super::*;

        /// IPC socket — used purely as a liveness probe.
        const SOCKET_PATH: &str = "/var/run/screentimed.sock";

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DaemonLifecycleState {
            NotRegistered,
            RequiresApproval,
            EnabledHealthy,
            EnabledMissingSocket,
            EnabledMissingAdminEndpoint,
        }

        /// Returns true iff the daemon is currently accepting connections.
        /// Cheap; no privileges required. The connection is closed
        /// immediately — we don't speak the protocol.
        pub fn daemon_socket_reachable() -> bool {
            std::os::unix::net::UnixStream::connect(SOCKET_PATH).is_ok()
        }

        pub fn daemon_lifecycle_state() -> Result<DaemonLifecycleState> {
            let status = super::service_management::daemon_status()?;
            let socket_reachable = daemon_socket_reachable();
            let admin_reachable = socket_reachable && admin_control_reachable();
            Ok(classify_daemon_lifecycle(
                status,
                socket_reachable,
                admin_reachable,
            ))
        }

        fn classify_daemon_lifecycle(
            status: super::service_management::Status,
            socket_reachable: bool,
            admin_reachable: bool,
        ) -> DaemonLifecycleState {
            use super::service_management::Status;

            match status {
                Status::NotRegistered | Status::NotFound | Status::Unknown(_) => {
                    DaemonLifecycleState::NotRegistered
                }
                Status::RequiresApproval => DaemonLifecycleState::RequiresApproval,
                Status::Enabled if !socket_reachable => DaemonLifecycleState::EnabledMissingSocket,
                Status::Enabled if !admin_reachable => {
                    DaemonLifecycleState::EnabledMissingAdminEndpoint
                }
                Status::Enabled => DaemonLifecycleState::EnabledHealthy,
            }
        }

        /// Returns true if the signed admin XPC control plane is usable.
        /// Any daemon-level response, including Unauthorized for a
        /// non-admin user, proves the Mach service is registered and the
        /// Rust handler received the request.
        pub fn admin_control_reachable() -> bool {
            for attempt in 1..=3 {
                match AdminClient::send_with_timeout(
                    AdminRequest::GetDaemonInfo,
                    Duration::from_secs(1),
                ) {
                    Ok(AdminResponse::DaemonInfo { .. } | AdminResponse::Unauthorized { .. }) => {
                        return true;
                    }
                    Ok(other) => {
                        tracing::debug!(attempt, ?other, "unexpected admin XPC health response");
                    }
                    Err(e) => {
                        tracing::debug!(attempt, error = %e, "admin XPC health check failed");
                    }
                }
                if attempt < 3 {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            false
        }

        pub fn ensure_user_agent(mtm: MainThreadMarker) {
            match super::service_management::ensure_agent_registered() {
                Ok(super::service_management::Status::Enabled) => {
                    tracing::info!("SMAppService tray agent enabled");
                }
                Ok(super::service_management::Status::RequiresApproval) => {
                    tracing::warn!("SMAppService tray agent requires approval");
                    super::service_management::open_login_items_settings();
                    alerts::message(
                        mtm,
                        "Login Startup Approval Needed",
                        "Enable Konstantin in System Settings so its menu-bar app starts after login.",
                    );
                }
                Ok(status) => {
                    tracing::warn!(?status, "unexpected SMAppService tray-agent status");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "SMAppService tray-agent registration failed");
                    alerts::message(
                        mtm,
                        "Login Startup Setup Failed",
                        &format!("macOS couldn't register Konstantin's login agent.\n\n{e}"),
                    );
                }
            }
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

            if paths.source != bundle::Source::Bundle {
                alerts::message(
                    mtm,
                    "Build App First",
                    "Konstantin setup is only available from the packaged app bundle. \
                     For development, build the release binaries, run \
                     ./packaging/build-app.sh, then launch target/Konstantin.app.",
                );
                return false;
            }

            run_smappservice_install(mtm)
        }

        pub fn maybe_run_admin_control_repair(mtm: MainThreadMarker) {
            match super::service_management::daemon_status() {
                Ok(super::service_management::Status::RequiresApproval) => {
                    super::service_management::open_login_items_settings();
                    alerts::message(
                        mtm,
                        "Approval Needed",
                        "Enable Konstantin in System Settings, then open Konstantin again.",
                    );
                    return;
                }
                Ok(super::service_management::Status::Enabled) => {}
                Ok(status) => {
                    tracing::warn!(?status, "admin XPC unavailable but daemon is not enabled");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not inspect daemon registration for repair");
                    return;
                }
            }
            match konstantin_tray::users::current_user_is_admin() {
                Ok(true) => {
                    let _ = run_admin_control_repair(mtm);
                }
                Ok(false) => {
                    tracing::warn!(
                        "admin XPC unavailable; skipping startup repair prompt for non-admin user"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "admin XPC unavailable; could not determine current user's admin status"
                    );
                }
            }
        }

        pub fn maybe_refresh_daemon_version(mtm: MainThreadMarker) {
            let response =
                AdminClient::send_with_timeout(AdminRequest::GetDaemonInfo, Duration::from_secs(2));
            let running = match response {
                Ok(AdminResponse::DaemonInfo { version }) => version,
                Ok(AdminResponse::Unauthorized { .. }) => return,
                Ok(other) => {
                    tracing::warn!(?other, "unexpected daemon-info response");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not query running daemon version");
                    return;
                }
            };

            let bundled = env!("CARGO_PKG_VERSION");
            if running == bundled {
                return;
            }
            tracing::info!(%running, %bundled, "refreshing daemon registration after bundle update");
            if bundle::Paths::resolve()
                .map(|p| p.source != bundle::Source::Bundle)
                .unwrap_or(true)
            {
                tracing::warn!("refusing daemon-version refresh outside an app bundle");
                return;
            }
            let _ = run_smappservice_repair(mtm);
        }

        fn run_admin_control_repair(mtm: MainThreadMarker) -> bool {
            if !alerts::confirm(
                mtm,
                "Repair Konstantin",
                "Konstantin's background service is running, but its administrator control \
                 channel is unavailable. macOS may ask an administrator to refresh the service.",
                "Repair…",
                "Later",
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
            if paths.source != bundle::Source::Bundle {
                alerts::message(
                    mtm,
                    "Build App First",
                    "Konstantin service repair is only available from the packaged app bundle.",
                );
                return false;
            }

            run_smappservice_repair(mtm)
        }

        fn run_smappservice_install(mtm: MainThreadMarker) -> bool {
            let result = super::progress::run_with_panel(
                mtm,
                "Setting Up Konstantin",
                "Registering Konstantin's background service…",
                "konstantin-smappservice-register",
                register_and_verify_daemon,
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
                    tracing::warn!(error = %e, "SMAppService install failed");
                    alerts::message(
                        mtm,
                        "Konstantin install failed.",
                        &format!("macOS couldn't register the bundled service with ServiceManagement.\n\n{e}"),
                    );
                    false
                }
            }
        }

        fn run_smappservice_repair(mtm: MainThreadMarker) -> bool {
            let result = super::progress::run_with_panel(
                mtm,
                "Repairing Konstantin",
                "Refreshing Konstantin's background service…",
                "konstantin-smappservice-repair",
                refresh_and_verify_daemon,
            );

            match result {
                Ok(super::service_management::Status::Enabled) => {
                    tracing::info!("SMAppService daemon registration refreshed");
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
                    tracing::warn!(
                        ?status,
                        "unexpected SMAppService daemon status after repair"
                    );
                    alerts::message(
                        mtm,
                        "Konstantin repair needs attention.",
                        &format!("Unexpected ServiceManagement status: {status:?}"),
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(error = %e, "SMAppService repair failed");
                    alerts::message(
                        mtm,
                        "Konstantin repair failed.",
                        &format!(
                            "macOS couldn't refresh the bundled service with ServiceManagement.\n\n{e}"
                        ),
                    );
                    false
                }
            }
        }

        fn register_and_verify_daemon() -> Result<super::service_management::Status> {
            let status = super::service_management::register_daemon()?;
            verify_enabled_daemon(status)
        }

        fn refresh_and_verify_daemon() -> Result<super::service_management::Status> {
            let status = super::service_management::refresh_daemon_registration()?;
            verify_enabled_daemon(status)
        }

        fn verify_enabled_daemon(
            status: super::service_management::Status,
        ) -> Result<super::service_management::Status> {
            if status != super::service_management::Status::Enabled {
                return Ok(status);
            }

            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if daemon_socket_reachable() {
                    if let Ok(AdminResponse::DaemonInfo { version }) =
                        AdminClient::send_with_timeout(
                            AdminRequest::GetDaemonInfo,
                            Duration::from_secs(1),
                        )
                    {
                        if version == env!("CARGO_PKG_VERSION") {
                            return Ok(status);
                        }
                    }
                }
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "registered daemon did not expose the expected socket, version, and admin endpoint"
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::imp::service_management::Status;

            #[test]
            fn classifies_daemon_lifecycle_states() {
                assert_eq!(
                    classify_daemon_lifecycle(Status::NotRegistered, false, false),
                    DaemonLifecycleState::NotRegistered
                );
                assert_eq!(
                    classify_daemon_lifecycle(Status::RequiresApproval, false, false),
                    DaemonLifecycleState::RequiresApproval
                );
                assert_eq!(
                    classify_daemon_lifecycle(Status::Enabled, false, false),
                    DaemonLifecycleState::EnabledMissingSocket
                );
                assert_eq!(
                    classify_daemon_lifecycle(Status::Enabled, true, false),
                    DaemonLifecycleState::EnabledMissingAdminEndpoint
                );
                assert_eq!(
                    classify_daemon_lifecycle(Status::Enabled, true, true),
                    DaemonLifecycleState::EnabledHealthy
                );
            }
        }
    }

    /// Native settings window. Replaces the previous "open the TOML in
    /// a text editor" flow with an AppKit window listing every real
    /// local user, with per-user daily-limit controls plus an editable
    /// warn-thresholds field at the top.
    ///
    /// Configure Open and Save use the daemon's signed admin XPC control
    /// plane. The root daemon reads/writes `/etc/screentimed/config.toml`,
    /// writes the root-owned configuration and reloads itself after a
    /// successful save. Tray startup is user-scoped SMAppService state
    /// and is deliberately not administered across accounts here.
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

            let open_response = super::progress::run_with_panel(
                mtm,
                "Open Configuration",
                "Reading /etc/screentimed/config.toml…",
                "konstantin-config-open",
                move || AdminClient::send(AdminRequest::GetConfig),
            );

            let config_text = match open_response {
                Ok(AdminResponse::Config { toml, .. }) => toml,
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
            let user_initials = collect_user_settings(&users_list, &config_value);

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
        }

        fn collect_user_settings(
            users: &[LocalUser],
            cfg: &toml::Value,
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

            BuiltRow {
                container,
                limit_check,
                minutes_field,
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
            limited: bool,
            minutes: u32,
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

            let outcome = super::progress::run_with_panel(
                mtm,
                "Saving Settings",
                "Saving and reloading Konstantin…",
                "konstantin-config-save",
                move || {
                    AdminClient::send(AdminRequest::SetConfig {
                        toml: new_config_text,
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
                        let minutes = r
                            .minutes_field
                            .stringValue()
                            .to_string()
                            .trim()
                            .parse::<u32>()
                            .unwrap_or(0);
                        RowSnapshot {
                            username: r.user.username.clone(),
                            limited,
                            minutes,
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
                        limited: true,
                        minutes: 90,
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
                        limited: true,
                        minutes: 0,
                    }],
                    config_value: cfg,
                };
                assert!(build_new_config_toml(&snap).is_err());
            }
        }
    }
}
