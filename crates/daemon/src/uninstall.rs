//! Bundle-existence watcher and self-uninstall.
//!
//! At install time, the tray writes the `.app` bundle's absolute path
//! into `/etc/screentimed/bundle_path`. This daemon reads that marker
//! on startup and, if present, polls the bundle path every tick. When
//! the bundle has been missing for `THRESHOLD_TICKS` consecutive
//! ticks (defaults to 60 s at the 5 s default tick), the daemon
//! triggers [`self_uninstall`] — drag-to-Trash on the operator's
//! `/Applications/Konstantin.app` becomes a real uninstall instead of
//! leaving a zombie root daemon enforcing limits.
//!
//! Dev-mode (`cargo run`, `target/release/screentimed`) doesn't write
//! the marker, so the watcher silently no-ops.

use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Default location of the install-time bundle-path marker. Written
/// by `crates/tray/src/bin/tray.rs::install::build_install_script`.
pub const MARKER_PATH: &str = "/etc/screentimed/bundle_path";

/// Consecutive missing-bundle ticks before we trip. At the default
/// 5 s tick this is 60 s — long enough to ride out APFS snapshot
/// remounts and brief network-volume blips, short enough that an
/// operator who Trashed the app sees the daemon stop within a minute.
pub const THRESHOLD_TICKS: u32 = 12;

pub struct BundleWatcher {
    bundle_path: Option<PathBuf>,
    consecutive_missing: u32,
    threshold: u32,
}

impl BundleWatcher {
    /// Read the marker file at `marker`. If it's missing, empty, or
    /// unreadable, the watcher is constructed in disabled mode and
    /// `tick` will always return `false`.
    pub fn from_marker(marker: &Path) -> Self {
        let bundle_path = std::fs::read_to_string(marker)
            .ok()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        if let Some(p) = &bundle_path {
            info!(bundle = %p.display(), "bundle watcher armed");
        } else {
            info!(marker = %marker.display(), "bundle watcher disabled (no marker)");
        }
        Self {
            bundle_path,
            consecutive_missing: 0,
            threshold: THRESHOLD_TICKS,
        }
    }

    /// Returns `true` exactly once when the threshold has been
    /// reached. The caller should respond by invoking
    /// [`self_uninstall`] (which does not return).
    pub fn tick(&mut self) -> bool {
        let Some(p) = self.bundle_path.as_ref() else {
            return false;
        };
        // `try_exists` distinguishes "definitely absent" from
        // "couldn't tell" (e.g. permission denied on a parent). We
        // only count *definitely absent*; a transient error counts
        // as present so we don't false-trip on weird filesystems.
        let definitely_missing = matches!(p.try_exists(), Ok(false));
        self.observe(!definitely_missing)
    }

    /// Pure accounting half of `tick`, exposed for unit tests.
    fn observe(&mut self, present: bool) -> bool {
        if present {
            if self.consecutive_missing > 0 {
                info!(
                    after_misses = self.consecutive_missing,
                    "bundle reappeared; resetting watcher counter"
                );
            }
            self.consecutive_missing = 0;
            return false;
        }
        self.consecutive_missing += 1;
        if self.consecutive_missing == self.threshold {
            warn!(
                bundle = ?self.bundle_path,
                missing_ticks = self.consecutive_missing,
                "bundle missing past threshold; triggering self-uninstall"
            );
            return true;
        }
        false
    }
}

/// Perform the system-side teardown and exit the process. Does not
/// return.
///
/// Order matters: we remove the binary first so launchd's
/// `KeepAlive=true` can't respawn us, then the LaunchDaemon plist
/// (so a reboot doesn't try to re-load the unit), then per-user tray
/// LaunchAgents, then runtime crumbs (sockets, marker, helper bins).
/// Finally we ask launchd to bootout our own service and exit.
///
/// All `rm`s are best-effort — a missing file just means a partial
/// install; we still want to finish the teardown.
pub fn self_uninstall() -> ! {
    info!("self-uninstall starting");

    // 1. Our own binary first. The kernel keeps the running file
    //    available via the open inode, but `KeepAlive=true` can no
    //    longer find a binary to relaunch, so this is what makes
    //    "exit and stay dead" work.
    rm_f("/usr/local/libexec/screentimed");

    // 2. LaunchDaemon plist. After this, a reboot won't try to load us.
    rm_f("/Library/LaunchDaemons/com.gitopolis.screentimed.plist");

    // 3. Per-user tray LaunchAgent plists. Iterate `/Users/*` and
    //    delete each one we find. Other users' running trays will
    //    already be confused (their bundle exe is gone), but the
    //    plist removal stops them from being respawned at next login.
    if let Ok(entries) = std::fs::read_dir("/Users") {
        for entry in entries.flatten() {
            let plist = entry
                .path()
                .join("Library/LaunchAgents/com.gitopolis.konstantin-tray.plist");
            if plist.exists() {
                rm_f_path(&plist);
            }
        }
    }
    // Legacy system-level location from pre-phase-7 installs.
    rm_f("/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist");

    // 4. Helper binaries from the dev-tree install path.
    rm_f("/usr/local/bin/konstantin-status");
    rm_f("/usr/local/bin/konstantin-tray");

    // 5. Runtime crumbs.
    rm_f("/var/run/screentimed.sock");
    rm_f(MARKER_PATH);

    // 6. Bootout self. Best-effort — even if the call fails or races
    //    with our exit, the binary and plist are already gone, so
    //    launchd won't bring us back.
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", "system/com.gitopolis.screentimed"])
        .status();

    info!("self-uninstall complete; exiting");
    std::process::exit(0);
}

fn rm_f(path: &str) {
    rm_f_path(Path::new(path));
}

fn rm_f_path(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => info!(path = %path.display(), "removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "remove failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watcher(threshold: u32) -> BundleWatcher {
        BundleWatcher {
            bundle_path: Some(PathBuf::from("/Applications/Test.app")),
            consecutive_missing: 0,
            threshold,
        }
    }

    #[test]
    fn disabled_watcher_never_trips() {
        let mut w = BundleWatcher {
            bundle_path: None,
            consecutive_missing: 0,
            threshold: 1,
        };
        for _ in 0..100 {
            assert!(!w.tick());
        }
    }

    #[test]
    fn trips_at_threshold_when_missing() {
        let mut w = watcher(3);
        assert!(!w.observe(false)); // 1
        assert!(!w.observe(false)); // 2
        assert!(w.observe(false)); // 3 → trip
    }

    #[test]
    fn does_not_trip_again_after_first_trip() {
        // We trip exactly once at the threshold and then keep
        // returning false; the caller is expected to invoke
        // self_uninstall on the trip and not call us again.
        let mut w = watcher(2);
        assert!(!w.observe(false));
        assert!(w.observe(false));
        assert!(!w.observe(false));
        assert!(!w.observe(false));
    }

    #[test]
    fn presence_resets_counter() {
        let mut w = watcher(3);
        assert!(!w.observe(false)); // 1
        assert!(!w.observe(false)); // 2
        assert!(!w.observe(true)); // present → reset
        assert!(!w.observe(false)); // 1 again
        assert!(!w.observe(false)); // 2
        assert!(w.observe(false)); // 3 → trip
    }

    #[test]
    fn from_marker_missing_file_disables_watcher() {
        let mut w = BundleWatcher::from_marker(Path::new("/nonexistent/bundle_path"));
        assert!(w.bundle_path.is_none());
        assert!(!w.tick());
    }

    #[test]
    fn from_marker_reads_and_trims() {
        let dir = std::env::temp_dir().join(format!(
            "konstantin-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("bundle_path");
        std::fs::write(&marker, "  /Applications/Konstantin.app\n").unwrap();

        let w = BundleWatcher::from_marker(&marker);
        assert_eq!(
            w.bundle_path.as_deref(),
            Some(Path::new("/Applications/Konstantin.app"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_marker_empty_file_disables_watcher() {
        let dir = std::env::temp_dir().join(format!(
            "konstantin-marker-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("bundle_path");
        std::fs::write(&marker, "").unwrap();

        let w = BundleWatcher::from_marker(&marker);
        assert!(w.bundle_path.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
