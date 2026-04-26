//! Persistent per-user counter state.
//!
//! On-disk shape (`state.json`):
//!
//! ```json
//! {
//!   "day": "2026-04-26",
//!   "counters": { "alice": 42, "bob": 0 }
//! }
//! ```
//!
//! Updates are atomic: serialize to `<path>.tmp`, then `rename(2)` over the
//! real file. `active_now` is in-memory only — recomputed each tick from
//! `utmpx` so a daemon crash doesn't carry a stale "is logged in" flag.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::warn;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Local calendar date these counters belong to. Once `today` advances
    /// past this, counters are zeroed.
    pub day: Option<NaiveDate>,

    /// `username -> seconds counted today`. Missing entries are treated as 0.
    #[serde(default)]
    pub counters: HashMap<String, u32>,

    /// Snapshot of usernames with a console session at the last tick. Not
    /// persisted (`#[serde(skip)]`) so a stale file can't claim someone is
    /// online.
    #[serde(skip)]
    pub active_now: HashSet<String>,
}

impl State {
    /// Load from `path`. Missing or unparseable file → fresh state (logged).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, path = %path.display(),
                        "state file unparseable, starting fresh");
                    State::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                warn!(error = %e, path = %path.display(),
                    "state read failed, starting fresh");
                State::default()
            }
        }
    }

    /// Atomically write to `path` via `<path>.tmp` + rename.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating state dir {}", parent.display()))?;
            }
        }
        let tmp = {
            let mut p = path.as_os_str().to_owned();
            p.push(".tmp");
            std::path::PathBuf::from(p)
        };
        let body = serde_json::to_vec_pretty(self).context("serializing state")?;
        std::fs::write(&tmp, &body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Zero counters if `today` differs from `self.day`. Returns `true` on
    /// rollover so the caller can log it.
    pub fn reset_if_new_day(&mut self, today: NaiveDate) -> bool {
        match self.day {
            Some(d) if d == today => false,
            _ => {
                self.day = Some(today);
                self.counters.clear();
                true
            }
        }
    }

    /// Add `delta_seconds` to every user in `active`; replace `active_now`.
    pub fn tick(&mut self, active: &HashSet<String>, delta_seconds: u32) {
        for user in active {
            *self.counters.entry(user.clone()).or_insert(0) += delta_seconds;
        }
        self.active_now = active.clone();
    }

    pub fn used(&self, username: &str) -> u32 {
        self.counters.get(username).copied().unwrap_or(0)
    }

    pub fn is_active(&self, username: &str) -> bool {
        self.active_now.contains(username)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_increments_only_active_users() {
        let mut s = State::default();
        let mut active = HashSet::new();
        active.insert("alice".to_string());
        s.tick(&active, 5);
        s.tick(&active, 5);
        assert_eq!(s.used("alice"), 10);
        assert_eq!(s.used("bob"), 0);
        assert!(s.is_active("alice"));
    }

    #[test]
    fn day_rollover_clears_counters() {
        let mut s = State::default();
        let mut active = HashSet::new();
        active.insert("alice".to_string());
        s.tick(&active, 60);
        let today = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
        let tomorrow = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        assert!(s.reset_if_new_day(today));
        assert_eq!(s.used("alice"), 0);
        assert!(!s.reset_if_new_day(today));
        assert!(s.reset_if_new_day(tomorrow));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir();
        let path = dir.join("state.json");
        let mut s = State::default();
        let mut active = HashSet::new();
        active.insert("alice".to_string());
        s.tick(&active, 42);
        s.day = Some(NaiveDate::from_ymd_opt(2026, 4, 26).unwrap());
        s.save_atomic(&path).unwrap();

        let loaded = State::load(&path);
        assert_eq!(loaded.used("alice"), 42);
        assert_eq!(loaded.day, s.day);
        // active_now is `#[serde(skip)]` — must be empty after reload.
        assert!(loaded.active_now.is_empty());
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "screentimed-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
