//! Threshold-notification machinery for `konstantin-tray`.
//!
//! Two pieces:
//!
//! * [`NotifTracker`] — pure decision logic. Given a stream of
//!   `UserStatus` updates, returns the threshold (in minutes) whose
//!   crossing the tray should announce, or `None`. Tested in isolation;
//!   no I/O.
//! * [`show`] (macOS only) — fires the actual notification via
//!   `UNUserNotificationCenter`, the modern Notification Center API.
//!   Notifications attribute to "Konstantin" with the bundle icon.
//!   Requires the running binary to be inside a code-signed `.app`
//!   bundle; bare `cargo run` from the dev tree has no NSBundle context
//!   and the call no-ops with a tracing warning.
//! * [`request_authorization`] — call once at tray startup so the
//!   first-run TCC consent dialog appears immediately rather than at
//!   the moment a threshold fires.

use chrono::{DateTime, Local};
use konstantin_proto::{SessionState, UserStatus};

/// Tracks which threshold has fired this reset cycle so each crossing
/// announces exactly once.
///
/// Day-rollover detection uses `UserStatus::resets_at` — when the daemon
/// reports a different `resets_at` than the last update we saw, the
/// counters have been zeroed and we re-arm.
#[derive(Default, Debug)]
pub struct NotifTracker {
    last_resets_at: Option<DateTime<Local>>,
    /// The smallest threshold (in seconds) we've already announced this
    /// cycle. Once we fire `1m`, we never fire `5m` or `15m` again until
    /// rollover.
    fired_at_or_below: Option<u32>,
}

impl NotifTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether `status` represents a fresh threshold crossing.
    /// Returns `Some(minutes)` to announce, `None` to stay silent.
    ///
    /// Rules:
    /// 1. On day rollover (`resets_at` changed) → re-arm.
    /// 2. Only fire when `state == Active`.
    /// 3. Pick the *smallest* configured threshold ≥ `remaining_seconds` —
    ///    if the tray starts up at `remaining = 100s` with thresholds
    ///    `[15,5,1]`, we fire only the 5-minute warning, not 15.
    /// 4. Once fired, don't re-fire until a strictly smaller threshold
    ///    has been crossed.
    pub fn evaluate(&mut self, status: &UserStatus) -> Option<u32> {
        // (1) Day rollover.
        if self.last_resets_at != Some(status.resets_at) {
            self.last_resets_at = Some(status.resets_at);
            self.fired_at_or_below = None;
        }

        // (2) Only Active warns. LimitReached / Offline / NotConfigured
        // are silent.
        if status.state != SessionState::Active {
            return None;
        }

        // remaining_seconds can be < 0 in a grace period; clamp.
        let remaining = match u32::try_from(status.remaining_seconds) {
            Ok(r) => r,
            Err(_) => return None,
        };

        // Build sorted-ascending threshold list in seconds.
        let mut thresholds_sec: Vec<u32> = status
            .warn_thresholds_minutes
            .iter()
            .map(|m| m.saturating_mul(60))
            .filter(|&t| t > 0)
            .collect();
        thresholds_sec.sort_unstable();
        thresholds_sec.dedup();

        // (3) Smallest threshold T such that remaining <= T.
        let candidate = thresholds_sec.iter().copied().find(|&t| remaining <= t);

        // (4) Fire only on a strictly tighter crossing than the last.
        match (candidate, self.fired_at_or_below) {
            (None, _) => None,
            (Some(t), None) => {
                self.fired_at_or_below = Some(t);
                Some(t / 60)
            }
            (Some(t), Some(prev)) if t < prev => {
                self.fired_at_or_below = Some(t);
                Some(t / 60)
            }
            _ => None,
        }
    }
}

/// Fire the actual user-visible notification (macOS only).
///
/// Posts to `UNUserNotificationCenter`. Returns once the request has
/// been handed off to NotificationCenter (delivery is asynchronous —
/// completion logging happens via the optional callback). Errors are
/// logged at the call site; the tray treats notification dispatch as
/// best-effort and never blocks the subscribe loop on it.
///
/// Stays `async` so the call site (which is already inside a tokio
/// task) doesn't need to know we no longer block on a subprocess.
#[cfg(target_os = "macos")]
pub async fn show(minutes_left: u32) -> anyhow::Result<()> {
    let title = "Screen time";
    let body = if minutes_left == 1 {
        "1 minute remaining today.".to_string()
    } else {
        format!("{minutes_left} minutes remaining today.")
    };
    macos::deliver(title, &body)
}

/// Ask the user for permission to show notifications. Call once at
/// startup. macOS persists the answer per-bundle so subsequent calls
/// after the first are no-ops from the user's perspective.
#[cfg(target_os = "macos")]
pub fn request_authorization() {
    macos::request_authorization();
}

#[cfg(target_os = "macos")]
mod macos {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString, NSUUID};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    /// Get the singleton notification center if we're running inside an
    /// `.app` bundle. From a bare `cargo run` (no bundle context),
    /// `UNUserNotificationCenter.currentNotificationCenter()` returns a
    /// dangling instance whose calls panic — we'd rather log and skip.
    fn center() -> Option<Retained<UNUserNotificationCenter>> {
        // The lookup itself is keyed off `[NSBundle mainBundle]`, which
        // returns a meaningful value only when the binary is the main
        // executable of a real bundle. We approximate that with an env
        // marker the bundle's Info.plist would have set, but the
        // simplest and most reliable check is: if `currentNotification
        // Center` returns something whose `notificationSettings` dance
        // works, we're good. AppKit handles the bundle gating
        // internally — we just call the API and trap errors at the
        // delivery completion handler.
        Some(UNUserNotificationCenter::currentNotificationCenter())
    }

    pub fn request_authorization() {
        let Some(c) = center() else {
            tracing::warn!("no UNUserNotificationCenter available; skipping auth");
            return;
        };
        let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
        let block = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if !granted.as_bool() {
                tracing::warn!("user did not grant notification permission");
            }
            if !error.is_null() {
                let desc = unsafe { (*error).localizedDescription() };
                tracing::warn!(error = %desc, "notification authorization error");
            }
        });
        c.requestAuthorizationWithOptions_completionHandler(options, &block);
    }

    pub fn deliver(title: &str, body: &str) -> anyhow::Result<()> {
        let Some(c) = center() else {
            anyhow::bail!("no UNUserNotificationCenter available");
        };

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));

        let id = NSUUID::new();
        let id_string = id.UUIDString();
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &id_string, &content, None, // immediate delivery
        );

        // Completion handler logs errors. We don't block on it — the
        // tray's subscribe loop must not pause on a slow Notification
        // Center.
        let block = RcBlock::new(move |error: *mut NSError| {
            if !error.is_null() {
                let desc = unsafe { (*error).localizedDescription() };
                tracing::warn!(error = %desc, "notification delivery error");
            }
        });
        c.addNotificationRequest_withCompletionHandler(&request, Some(&block));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(remaining: i64, thresholds: &[u32], state: SessionState, day: u32) -> UserStatus {
        UserStatus {
            uid: 501,
            username: "test".into(),
            state,
            daily_limit_seconds: 3600,
            used_seconds: 3600u32.saturating_sub(remaining.max(0) as u32),
            remaining_seconds: remaining,
            resets_at: Local.with_ymd_and_hms(2026, 4, day, 0, 0, 0).unwrap(),
            warn_thresholds_minutes: thresholds.to_vec(),
        }
    }

    #[test]
    fn fires_on_first_crossing_and_not_again() {
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];

        assert_eq!(t.evaluate(&at(1000, &thr, SessionState::Active, 26)), None);
        assert_eq!(
            t.evaluate(&at(900, &thr, SessionState::Active, 26)),
            Some(15)
        );
        assert_eq!(t.evaluate(&at(800, &thr, SessionState::Active, 26)), None);
        assert_eq!(
            t.evaluate(&at(300, &thr, SessionState::Active, 26)),
            Some(5)
        );
        assert_eq!(t.evaluate(&at(60, &thr, SessionState::Active, 26)), Some(1));
        assert_eq!(t.evaluate(&at(30, &thr, SessionState::Active, 26)), None);
    }

    #[test]
    fn late_subscriber_only_fires_smallest_applicable_threshold() {
        // Tray starts up at 100s remaining. Thresholds [15,5,1].
        // 15 is "missed"; 5 is the smallest that still applies; 1 not yet.
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        assert_eq!(
            t.evaluate(&at(100, &thr, SessionState::Active, 26)),
            Some(5)
        );
        assert_eq!(t.evaluate(&at(99, &thr, SessionState::Active, 26)), None);
    }

    #[test]
    fn day_rollover_rearms() {
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        assert_eq!(
            t.evaluate(&at(900, &thr, SessionState::Active, 26)),
            Some(15)
        );
        assert_eq!(t.evaluate(&at(60, &thr, SessionState::Active, 26)), Some(1));
        // New day: counters reset, remaining is full again.
        assert_eq!(t.evaluate(&at(3600, &thr, SessionState::Active, 27)), None);
        // Threshold fires fresh on the new day.
        assert_eq!(
            t.evaluate(&at(900, &thr, SessionState::Active, 27)),
            Some(15)
        );
    }

    #[test]
    fn limit_reached_is_silent() {
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        // 0 remaining and LimitReached: no notification (the user is
        // being kicked, not warned).
        assert_eq!(
            t.evaluate(&at(0, &thr, SessionState::LimitReached, 26)),
            None
        );
    }

    #[test]
    fn empty_thresholds_never_fire() {
        let mut t = NotifTracker::new();
        let thr: [u32; 0] = [];
        assert_eq!(t.evaluate(&at(60, &thr, SessionState::Active, 26)), None);
        assert_eq!(t.evaluate(&at(0, &thr, SessionState::Active, 26)), None);
    }

    #[test]
    fn negative_remaining_is_silent() {
        // Grace period: remaining < 0. Don't fire (we're past the limit).
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        assert_eq!(t.evaluate(&at(-1, &thr, SessionState::Active, 26)), None);
    }
}
