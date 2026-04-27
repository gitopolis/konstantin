//! Enforcement: decide who (if anyone) needs to be booted out, and do it.
//!
//! Two-step design — `decide` is pure and unit-testable; `act_on` performs
//! the side effect (`launchctl bootout`). Wiring lives in [`Enforcer::step`].
//!
//! Safety-critical paths gated here:
//!   * `Enforcement::Log` (default in `config.example.toml`) — never spawns
//!     `launchctl`; only logs `"would have kicked"`.
//!   * Kill-switch file — if `cfg.kill_switch_path` exists, we skip the
//!     bootout even when `enforcement = "logout"`. Touch the file to
//!     disable enforcement live; remove it to re-enable.
//!   * Per-uid backoff — once we kick (or pretend to), we don't try again
//!     for `BOOTOUT_BACKOFF`. Stops a tight kick → re-login → kick loop.

use crate::config::{Config, DefaultPolicy, Enforcement};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Don't re-bootout the same uid more often than this. CLAUDE.md decision
/// #1: "Add a short 'recently kicked' backoff (~10 s) so we don't
/// spam-bootout in a tight loop."
const BOOTOUT_BACKOFF: Duration = Duration::from_secs(10);

/// Cap on how long we'll wait for `launchctl bootout` to return before
/// giving up and moving on. bootout normally returns within milliseconds,
/// but we don't want a hung subprocess to wedge the ticker.
const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    NoOp,
    Kick(KickReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickReason {
    /// User is in `[users.*]` and has used at least their daily limit.
    LimitReached,
    /// User is not in `[users.*]` and `default_policy = "block"`.
    BlockedByDefaultPolicy,
}

/// Pure decision: should `username` be booted out right now?
///
/// `used` is their current counter; `now` is the wall-clock at decision
/// time (passed in so tests can fake it).
pub fn decide(
    cfg: &Config,
    username: &str,
    used: u32,
    last_kicked: Option<Instant>,
    now: Instant,
) -> Decision {
    // Backoff applies regardless of reason — if we just acted on this user,
    // hold off.
    if let Some(t) = last_kicked {
        if now.duration_since(t) < BOOTOUT_BACKOFF {
            return Decision::NoOp;
        }
    }

    match cfg.user_by_name(username) {
        Some(u) => {
            let limit = u.daily_limit_minutes.saturating_mul(60);
            if used >= limit {
                Decision::Kick(KickReason::LimitReached)
            } else {
                Decision::NoOp
            }
        }
        None => match cfg.default_policy {
            DefaultPolicy::Unrestricted => Decision::NoOp,
            DefaultPolicy::Block => Decision::Kick(KickReason::BlockedByDefaultPolicy),
        },
    }
}

pub struct Enforcer {
    last_kicked: HashMap<u32, Instant>,
}

impl Enforcer {
    pub fn new() -> Self {
        Self {
            last_kicked: HashMap::new(),
        }
    }

    /// Walk every active console user, decide, and (if needed) act.
    pub async fn step(
        &mut self,
        cfg: &Config,
        active: &HashSet<String>,
        counters: &HashMap<String, u32>,
    ) {
        let now = Instant::now();
        for username in active {
            let used = counters.get(username).copied().unwrap_or(0);
            let uid = match resolve_uid(username) {
                Some(u) => u,
                None => {
                    warn!(%username, "cannot resolve uid; skipping enforcement");
                    continue;
                }
            };
            let decision = decide(cfg, username, used, self.last_kicked.get(&uid).copied(), now);
            if let Decision::Kick(reason) = decision {
                self.act_on(cfg, username, uid, used, reason).await;
            }
        }
    }

    async fn act_on(
        &mut self,
        cfg: &Config,
        username: &str,
        uid: u32,
        used_s: u32,
        reason: KickReason,
    ) {
        // Kill-switch — checked even in Log mode so the touch-file behaves
        // identically regardless of which mode is configured.
        if cfg.kill_switch_path.exists() {
            warn!(
                path = %cfg.kill_switch_path.display(),
                %username, uid, ?reason,
                "kill-switch present, refusing to enforce"
            );
            return;
        }

        match cfg.enforcement {
            Enforcement::Log => {
                info!(
                    %username, uid, used_s, ?reason,
                    "would have kicked (enforcement=log)"
                );
                self.last_kicked.insert(uid, Instant::now());
            }
            Enforcement::Logout => {
                info!(
                    %username, uid, used_s, ?reason,
                    "kicking via launchctl bootout"
                );
                match bootout(uid).await {
                    Ok(()) => {
                        self.last_kicked.insert(uid, Instant::now());
                    }
                    Err(e) => {
                        warn!(error = %e, %username, uid, "bootout failed (will retry next tick)");
                        // Note: deliberately not stamping last_kicked on
                        // failure — we want to retry promptly.
                    }
                }
            }
        }
    }
}

fn resolve_uid(username: &str) -> Option<u32> {
    nix::unistd::User::from_name(username)
        .ok()
        .flatten()
        .map(|u| u.uid.as_raw())
}

async fn bootout(uid: u32) -> Result<()> {
    let target = format!("user/{uid}");
    let fut = tokio::process::Command::new("/bin/launchctl")
        .arg("bootout")
        .arg(&target)
        .status();
    let status = tokio::time::timeout(BOOTOUT_TIMEOUT, fut)
        .await
        .with_context(|| format!("launchctl bootout {target} timed out"))?
        .with_context(|| format!("spawning launchctl bootout {target}"))?;
    if !status.success() {
        anyhow::bail!("launchctl bootout {target} exited {}", status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use std::path::PathBuf;

    fn cfg_with(
        enforcement: Enforcement,
        default_policy: DefaultPolicy,
        users: &[(&str, u32)],
    ) -> Config {
        let mut map = HashMap::new();
        for (name, mins) in users {
            map.insert(
                (*name).into(),
                UserConfig {
                    daily_limit_minutes: *mins,
                },
            );
        }
        Config {
            socket_path: PathBuf::from("/tmp/.unused.sock"),
            state_path: PathBuf::from("/tmp/.unused.json"),
            tick_seconds: 5,
            warn_thresholds_minutes: vec![15, 5, 1],
            default_policy,
            enforcement,
            kill_switch_path: PathBuf::from("/tmp/.unused.disable"),
            users: map,
        }
    }

    #[test]
    fn configured_user_under_limit_is_noop() {
        let cfg = cfg_with(Enforcement::Logout, DefaultPolicy::Unrestricted, &[("alice", 30)]);
        let now = Instant::now();
        assert_eq!(decide(&cfg, "alice", 100, None, now), Decision::NoOp);
    }

    #[test]
    fn configured_user_at_limit_kicks_for_limit_reached() {
        let cfg = cfg_with(Enforcement::Logout, DefaultPolicy::Unrestricted, &[("alice", 1)]);
        let now = Instant::now();
        assert_eq!(
            decide(&cfg, "alice", 60, None, now),
            Decision::Kick(KickReason::LimitReached)
        );
        // Still kicks comfortably over the limit.
        assert_eq!(
            decide(&cfg, "alice", 99999, None, now),
            Decision::Kick(KickReason::LimitReached)
        );
    }

    #[test]
    fn unconfigured_user_under_unrestricted_is_noop() {
        let cfg = cfg_with(Enforcement::Logout, DefaultPolicy::Unrestricted, &[]);
        let now = Instant::now();
        assert_eq!(decide(&cfg, "stranger", 0, None, now), Decision::NoOp);
        assert_eq!(decide(&cfg, "stranger", 99999, None, now), Decision::NoOp);
    }

    #[test]
    fn unconfigured_user_under_block_kicks_for_default_policy() {
        let cfg = cfg_with(Enforcement::Logout, DefaultPolicy::Block, &[]);
        let now = Instant::now();
        assert_eq!(
            decide(&cfg, "stranger", 0, None, now),
            Decision::Kick(KickReason::BlockedByDefaultPolicy)
        );
    }

    #[test]
    fn backoff_suppresses_repeat_kick() {
        let cfg = cfg_with(Enforcement::Logout, DefaultPolicy::Unrestricted, &[("alice", 1)]);
        let now = Instant::now();
        // Just kicked 1 ms ago.
        let recent = now - Duration::from_millis(1);
        assert_eq!(decide(&cfg, "alice", 60, Some(recent), now), Decision::NoOp);
        // Kicked > backoff ago.
        let old = now - BOOTOUT_BACKOFF - Duration::from_millis(1);
        assert_eq!(
            decide(&cfg, "alice", 60, Some(old), now),
            Decision::Kick(KickReason::LimitReached)
        );
    }
}
