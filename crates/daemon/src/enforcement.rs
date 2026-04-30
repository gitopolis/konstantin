//! Enforcement: decide who (if anyone) needs to be booted out, and do it.
//!
//! Two-step design — `decide` is pure and unit-testable; `act_on` performs
//! the side effect (force-logout). Wiring lives in [`Enforcer::step`].
//!
//! Safety-critical paths gated here:
//!   * `Enforcement::Log` (compile-time default if the field is missing
//!     from `config.toml`) — never spawns `launchctl`; only logs
//!     `"would have kicked"`. The shipped `config.example.toml` sets
//!     `enforcement = "logout"` so a fresh install actually enforces.
//!   * Kill-switch file — if `cfg.kill_switch_path` exists, we skip the
//!     logout even when `enforcement = "logout"`. Touch the file to
//!     disable enforcement live; remove it to re-enable.
//!   * Per-uid backoff — once we kick (or pretend to), we don't try again
//!     for `BOOTOUT_BACKOFF`. Stops a tight kick → re-login → kick loop.
//!
//! ## Force-logout escalation
//!
//! `launchctl bootout user/<uid>` alone is not sufficient on macOS Tahoe:
//! production logs have shown it return success (exit 0) for 14+ hours
//! straight while the loginwindow session persists and `utmpx` keeps
//! reporting the user. So `force_logout` runs an escalation:
//!
//!   1. `launchctl bootout gui/<uid>`  — tear down the Aqua/GUI domain
//!   2. `launchctl bootout user/<uid>` — tear down LaunchAgents domain
//!   3. settle ~1 s, then re-enumerate `console_users()` from utmpx
//!   4. if the user is still listed: `pkill -KILL -U <uid>` (last resort)

use crate::config::{Config, DefaultPolicy, Enforcement};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Don't re-bootout the same uid more often than this. CLAUDE.md decision
/// #1: "Add a short 'recently kicked' backoff (~10 s) so we don't
/// spam-bootout in a tight loop."
const BOOTOUT_BACKOFF: Duration = Duration::from_secs(10);

/// Cap on how long we'll wait for any single subprocess in the logout
/// chain (`launchctl bootout`, `pkill`) to return before giving up.
/// bootout normally returns within milliseconds, but we don't want a
/// hung subprocess to wedge the ticker.
const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait between issuing the bootouts and re-checking
/// `utmpx`. macOS needs a beat to actually tear down loginwindow
/// after `launchctl bootout gui/<uid>` returns.
const SETTLE_AFTER_BOOTOUT: Duration = Duration::from_secs(1);

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
                    "forcing logout"
                );
                match force_logout(&RealRunner, username, uid).await {
                    Ok(()) => {
                        self.last_kicked.insert(uid, Instant::now());
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            %username, uid,
                            "force-logout failed (will retry next tick)"
                        );
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

/// Escalation: `launchctl bootout gui/<uid>` → `launchctl bootout user/<uid>`
/// → re-check `utmpx` → optional `pkill -KILL -U <uid>`.
///
/// Returns `Ok(())` if the user is no longer on a console session by the
/// time we return. Returns `Err` only when the final escalation step
/// (the `pkill`) actually fails — earlier steps are best-effort.
async fn force_logout<R: LogoutRunner + ?Sized>(
    runner: &R,
    username: &str,
    uid: u32,
) -> Result<()> {
    // Best-effort. We don't bail on failure here; the re-check below is
    // what tells us whether we're done.
    let _ = runner
        .run("/bin/launchctl", vec!["bootout".into(), format!("gui/{uid}")])
        .await
        .map_err(|e| {
            warn!(error = %e, %username, uid, step = "gui-bootout", "logout step error");
            e
        });
    let _ = runner
        .run("/bin/launchctl", vec!["bootout".into(), format!("user/{uid}")])
        .await
        .map_err(|e| {
            warn!(error = %e, %username, uid, step = "user-bootout", "logout step error");
            e
        });

    runner.settle(SETTLE_AFTER_BOOTOUT).await;

    let still_active = runner.console_users();
    if !still_active.contains(username) {
        info!(%username, uid, "logout via bootout succeeded");
        return Ok(());
    }

    // bootouts didn't end the session. Last resort: SIGKILL every
    // process owned by the uid. macOS will then reset the loginwindow
    // session for that account.
    warn!(
        %username, uid,
        "bootouts did not terminate session, escalating to pkill -KILL -U"
    );
    let ok = runner
        .run("/usr/bin/pkill", vec!["-KILL".into(), "-U".into(), uid.to_string()])
        .await
        .with_context(|| format!("pkill -KILL -U {uid}"))?;
    if !ok {
        // pkill exits 1 when no processes matched. If the user is *still*
        // in console_users after that, something is very wrong; report
        // it. If they're gone, treat as success.
        let after = runner.console_users();
        if after.contains(username) {
            anyhow::bail!("pkill -KILL -U {uid} matched nothing but user still active");
        }
    }
    info!(%username, uid, "logout via pkill succeeded");
    Ok(())
}

/// Side-effecting operations that `force_logout` depends on. Abstracted
/// behind a trait so tests can swap in a fake that records the call
/// sequence and returns canned results without spawning subprocesses or
/// reading utmpx.
trait LogoutRunner {
    async fn run(&self, program: &str, args: Vec<String>) -> Result<bool>;
    async fn settle(&self, dur: Duration);
    fn console_users(&self) -> HashSet<String>;
}

struct RealRunner;

impl LogoutRunner for RealRunner {
    async fn run(&self, program: &str, args: Vec<String>) -> Result<bool> {
        let fut = tokio::process::Command::new(program).args(&args).status();
        let status = tokio::time::timeout(BOOTOUT_TIMEOUT, fut)
            .await
            .with_context(|| format!("{program} {args:?} timed out"))?
            .with_context(|| format!("spawning {program} {args:?}"))?;
        Ok(status.success())
    }
    async fn settle(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
    fn console_users(&self) -> HashSet<String> {
        crate::sessions::console_users()
    }
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

    /// Test runner that records every `run` and `settle` call and returns
    /// canned values for `console_users` (one per call, popped in order)
    /// and `run` (one per call, popped in order; defaults to true).
    struct FakeRunner {
        inner: std::sync::Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<(String, Vec<String>)>,
        user_queries: std::collections::VecDeque<HashSet<String>>,
        run_results: std::collections::VecDeque<bool>,
        settled: Vec<Duration>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                inner: std::sync::Mutex::new(FakeState::default()),
            }
        }
        fn enqueue_users(&self, users: HashSet<String>) {
            self.inner.lock().unwrap().user_queries.push_back(users);
        }
        fn enqueue_run_result(&self, ok: bool) {
            self.inner.lock().unwrap().run_results.push_back(ok);
        }
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.inner.lock().unwrap().calls.clone()
        }
        fn settle_count(&self) -> usize {
            self.inner.lock().unwrap().settled.len()
        }
    }

    impl LogoutRunner for FakeRunner {
        async fn run(&self, program: &str, args: Vec<String>) -> Result<bool> {
            let mut s = self.inner.lock().unwrap();
            s.calls.push((program.to_string(), args));
            Ok(s.run_results.pop_front().unwrap_or(true))
        }
        async fn settle(&self, dur: Duration) {
            self.inner.lock().unwrap().settled.push(dur);
        }
        fn console_users(&self) -> HashSet<String> {
            self.inner
                .lock()
                .unwrap()
                .user_queries
                .pop_front()
                .unwrap_or_default()
        }
    }

    fn singleton(name: &str) -> HashSet<String> {
        let mut s = HashSet::new();
        s.insert(name.into());
        s
    }

    #[tokio::test]
    async fn force_logout_skips_pkill_when_session_clears() {
        let fake = FakeRunner::new();
        // After the two bootouts, utmpx no longer reports alice.
        fake.enqueue_users(HashSet::new());

        force_logout(&fake, "alice", 601).await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "should not have called pkill");
        assert_eq!(calls[0].0, "/bin/launchctl");
        assert_eq!(calls[0].1, vec!["bootout".to_string(), "gui/601".into()]);
        assert_eq!(calls[1].0, "/bin/launchctl");
        assert_eq!(calls[1].1, vec!["bootout".to_string(), "user/601".into()]);
        assert_eq!(fake.settle_count(), 1);
    }

    #[tokio::test]
    async fn force_logout_escalates_to_pkill_when_user_persists() {
        let fake = FakeRunner::new();
        // Even after both bootouts, alice is still reported on console.
        fake.enqueue_users(singleton("alice"));

        force_logout(&fake, "alice", 601).await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 3, "expected gui-bootout, user-bootout, pkill");
        assert_eq!(calls[2].0, "/usr/bin/pkill");
        assert_eq!(
            calls[2].1,
            vec!["-KILL".to_string(), "-U".into(), "601".into()]
        );
    }

    #[tokio::test]
    async fn force_logout_errors_when_pkill_no_match_and_user_remains() {
        let fake = FakeRunner::new();
        // First console_users() (after bootouts): alice still there → pkill.
        // Second console_users() (after pkill, only checked on non-success):
        // alice STILL there → that's the giving-up path.
        fake.enqueue_users(singleton("alice"));
        fake.enqueue_users(singleton("alice"));
        // Two bootouts succeed (default), but pkill returns non-success
        // (matched nothing).
        fake.enqueue_run_result(true);
        fake.enqueue_run_result(true);
        fake.enqueue_run_result(false);

        let result = force_logout(&fake, "alice", 601).await;
        assert!(result.is_err(), "expected force_logout to bail");
    }

    #[tokio::test]
    async fn force_logout_pkill_no_match_but_user_gone_is_ok() {
        // pkill exits 1 (no match) but the user is also no longer on a
        // console — they bailed mid-way. Treat as success, no panic.
        let fake = FakeRunner::new();
        fake.enqueue_users(singleton("alice")); // post-bootout: still there
        fake.enqueue_users(HashSet::new()); // post-pkill: gone
        fake.enqueue_run_result(true);
        fake.enqueue_run_result(true);
        fake.enqueue_run_result(false);

        force_logout(&fake, "alice", 601).await.unwrap();
    }
}
