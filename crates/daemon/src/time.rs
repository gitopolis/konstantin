//! Local-time helpers for the daemon.
//!
//! Centralised so the IPC handler and the midnight-reset task agree on
//! exactly when "tomorrow at 00:00" is. Importantly, `next_local_midnight`
//! always *recomputes* via `Local.from_local_datetime` — it never adds
//! 86400 seconds to the previous boundary, which would silently drift on
//! DST transitions.

use chrono::{DateTime, Duration, Local, LocalResult, NaiveDate, TimeZone};

/// The next local-time midnight strictly in the future relative to `now`.
///
/// On the (rare) timezones where 00:00 doesn't exist or is ambiguous on a
/// given calendar day we fall back sensibly: ambiguous → the earlier of the
/// two; nonexistent → we skip that day.
pub fn next_local_midnight() -> DateTime<Local> {
    next_local_midnight_after(Local::now())
}

fn next_local_midnight_after(now: DateTime<Local>) -> DateTime<Local> {
    let mut date: NaiveDate = (now + Duration::days(1)).date_naive();
    // Guard against pathological timezones with 1+ unrepresentable midnights.
    for _ in 0..7 {
        let candidate = date.and_hms_opt(0, 0, 0).expect("00:00:00 is valid");
        match Local.from_local_datetime(&candidate) {
            LocalResult::Single(dt) => return dt,
            LocalResult::Ambiguous(earlier, _later) => return earlier,
            LocalResult::None => {
                date += Duration::days(1);
                continue;
            }
        }
    }
    // Should be unreachable; bail out with "now + 24h" as an absolute last
    // resort so the caller doesn't busy-loop.
    now + Duration::days(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn next_midnight_is_in_the_future_and_at_midnight() {
        let now = Local::now();
        let next = next_local_midnight_after(now);
        assert!(next > now);
        assert_eq!(next.time().hour(), 0);
        assert_eq!(next.time().minute(), 0);
        assert_eq!(next.time().second(), 0);
    }

    #[test]
    fn next_midnight_within_48h() {
        // Even on the longest DST night the boundary is < 48h out.
        let now = Local::now();
        let next = next_local_midnight_after(now);
        let delta = next - now;
        assert!(delta < Duration::hours(48), "delta = {delta}");
        assert!(delta > Duration::zero());
    }
}
