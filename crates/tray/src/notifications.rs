//! Threshold-notification machinery for `screentime-tray`.
//!
//! Two pieces:
//!
//! * [`NotifTracker`] — pure decision logic. Given a stream of
//!   `UserStatus` updates, returns the threshold (in minutes) whose
//!   crossing the tray should announce, or `None`. Tested in isolation;
//!   no I/O.
//! * [`show`] (macOS only) — fires the actual notification by shelling
//!   out to `osascript`. Per CLAUDE.md: `osascript` is signed by Apple,
//!   so it works without the bundle being signed and without TCC
//!   consent. `UNUserNotificationCenter` is the longer-term path; this
//!   keeps phase 7 simple.

use chrono::{DateTime, Local};
use screentime_proto::{SessionState, UserStatus};

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
/// Returns when `osascript` exits. Errors are logged at the call site;
/// the tray treats notification dispatch as best-effort and never blocks
/// the subscribe loop on it.
#[cfg(target_os = "macos")]
pub async fn show(minutes_left: u32) -> anyhow::Result<()> {
    let title = "Screen time";
    let body = if minutes_left == 1 {
        "1 minute remaining today.".to_string()
    } else {
        format!("{minutes_left} minutes remaining today.")
    };

    // AppleScript double-quoted strings: escape `\` and `"`.
    let body_esc = applescript_escape(&body);
    let title_esc = applescript_escape(title);
    let script = format!(r#"display notification "{body_esc}" with title "{title_esc}""#);

    let status = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("osascript exited {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        assert_eq!(t.evaluate(&at(900, &thr, SessionState::Active, 26)), Some(15));
        assert_eq!(t.evaluate(&at(800, &thr, SessionState::Active, 26)), None);
        assert_eq!(t.evaluate(&at(300, &thr, SessionState::Active, 26)), Some(5));
        assert_eq!(t.evaluate(&at(60, &thr, SessionState::Active, 26)), Some(1));
        assert_eq!(t.evaluate(&at(30, &thr, SessionState::Active, 26)), None);
    }

    #[test]
    fn late_subscriber_only_fires_smallest_applicable_threshold() {
        // Tray starts up at 100s remaining. Thresholds [15,5,1].
        // 15 is "missed"; 5 is the smallest that still applies; 1 not yet.
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        assert_eq!(t.evaluate(&at(100, &thr, SessionState::Active, 26)), Some(5));
        assert_eq!(t.evaluate(&at(99, &thr, SessionState::Active, 26)), None);
    }

    #[test]
    fn day_rollover_rearms() {
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        assert_eq!(t.evaluate(&at(900, &thr, SessionState::Active, 26)), Some(15));
        assert_eq!(t.evaluate(&at(60, &thr, SessionState::Active, 26)), Some(1));
        // New day: counters reset, remaining is full again.
        assert_eq!(
            t.evaluate(&at(3600, &thr, SessionState::Active, 27)),
            None
        );
        // Threshold fires fresh on the new day.
        assert_eq!(t.evaluate(&at(900, &thr, SessionState::Active, 27)), Some(15));
    }

    #[test]
    fn limit_reached_is_silent() {
        let mut t = NotifTracker::new();
        let thr = [15, 5, 1];
        // 0 remaining and LimitReached: no notification (the user is
        // being kicked, not warned).
        assert_eq!(t.evaluate(&at(0, &thr, SessionState::LimitReached, 26)), None);
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
