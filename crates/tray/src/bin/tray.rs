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
        NSAlert, NSApplication, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
        NSVariableStatusItemLength,
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

        // Idempotent: write our per-user LaunchAgent plist so launchd
        // auto-starts the tray on next login. Doesn't bootstrap — we
        // ARE the running tray; bootstrap would race-spawn a sibling.
        if let Err(e) = install::ensure_user_launchagent() {
            tracing::warn!(error = %e, "user LaunchAgent setup failed (non-fatal)");
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
                .name("screentime-tray-admin".into())
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
        const SYSTEM_PLIST: &str = "/Library/LaunchDaemons/com.qnicks.screentimed.plist";
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
                "Set up Screentime",
                "Screentime needs to install its background service. \
                 You'll be prompted for your administrator password.",
                "Set Up…",
                "Quit",
            ) {
                return false;
            }
            let paths = match BundlePaths::resolve() {
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
                "Setting Up Screentime",
                "Installing Screentime's background service.\n\
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
                    alerts::message(mtm, "Screentime install failed.", &msg);
                    false
                }
            }
        }

        /// Idempotent. Writes `~/Library/LaunchAgents/com.qnicks.screentime-tray.plist`
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
            let dst = agents_dir.join("com.qnicks.screentime-tray.plist");
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

        /// Bundle-relative paths to the daemon binary, daemon plist
        /// template, and example config. Falls back to dev-tree paths
        /// (under `target/release/` and `packaging/`) when the binary
        /// isn't running from a `.app` bundle so `cargo run` workflows
        /// still work.
        struct BundlePaths {
            daemon_binary: PathBuf,
            daemon_plist: PathBuf,
            config_example: PathBuf,
        }

        impl BundlePaths {
            fn resolve() -> anyhow::Result<Self> {
                let exe = std::env::current_exe()?;
                let macos_dir = exe
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("exe has no parent"))?;
                let contents = macos_dir
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("MacOS/ has no parent"))?;

                // Bundle layout: Contents/Resources/screentimed exists.
                let resources = contents.join("Resources");
                let bundled_daemon = resources.join("screentimed");
                if bundled_daemon.is_file() {
                    return Ok(Self {
                        daemon_binary: bundled_daemon,
                        daemon_plist: contents
                            .join("Library/LaunchDaemons/com.qnicks.screentimed.plist"),
                        config_example: resources.join("config.example.toml"),
                    });
                }

                // Dev fallback: target/release/screentime-tray + packaging/.
                let release = macos_dir;
                let workspace = release
                    .parent()
                    .and_then(|p| p.parent())
                    .ok_or_else(|| anyhow::anyhow!("can't find workspace root from {}", exe.display()))?;
                Ok(Self {
                    daemon_binary: release.join("screentimed"),
                    daemon_plist: workspace.join("packaging/com.qnicks.screentimed.plist"),
                    config_example: workspace.join("packaging/config.example.toml"),
                })
            }
        }

        fn build_install_script(p: &BundlePaths) -> String {
            // Single bash command via `&&` chains. `install -d` creates
            // missing dirs idempotently. Re-running is safe — `cp`
            // overwrites the daemon binary (handles upgrades), and the
            // config copy is guarded by a `[ -f ... ] ||` so an existing
            // `/etc/screentimed/config.toml` is never trampled.
            //
            // `launchctl bootstrap` is ORed with `true` because it fails
            // if the service is already loaded — kickstart -k afterwards
            // forces a restart either way.
            format!(
                "install -d -m 0755 /usr/local/libexec && \
                 install -d -m 0755 /etc/screentimed && \
                 install -d -m 0700 /var/db/screentimed && \
                 install -m 0755 '{daemon}' /usr/local/libexec/screentimed && \
                 install -m 0644 '{plist}' /Library/LaunchDaemons/com.qnicks.screentimed.plist && \
                 ([ -f /etc/screentimed/config.toml ] || install -m 0644 '{config}' /etc/screentimed/config.toml) && \
                 (launchctl bootstrap system /Library/LaunchDaemons/com.qnicks.screentimed.plist || true) && \
                 launchctl enable system/com.qnicks.screentimed && \
                 launchctl kickstart -k system/com.qnicks.screentimed",
                daemon = p.daemon_binary.display(),
                plist = p.daemon_plist.display(),
                config = p.config_example.display(),
            )
        }

        fn build_user_launchagent_plist(tray_exe: &Path) -> String {
            let exe = xml_escape(&tray_exe.display().to_string());
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.qnicks.screentime-tray</string>
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
    <string>/tmp/screentime-tray.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/screentime-tray.err.log</string>
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
    }
}
