# Concepts

How `screentimed` works under the hood. Read this before changing the
enforcement path or adding new wire types.

## Three artifacts, one workspace

`screentimed` (privileged daemon, runs as root)
* asks the system who's at the console via
  `SCDynamicStoreCopyConsoleUser`
* tracks per-user counters and persists `state.json`
* runs the midnight reset task
* serves IPC (one-shot `GetStatus` + long-lived `Subscribe`)
* invokes `launchctl bootout` when configured to enforce

`konstantin-status` (headless CLI, any user)
* one-shot status via `GetStatus`
* `--watch` mode subscribes and streams live updates

`konstantin-tray` (per-user menu-bar app, Aqua only)
* `NSStatusItem` showing remaining time, updated live from `Subscribe`
* current-thread tokio runtime on a worker thread; main thread runs
  `NSApplication` and a 5 Hz `NSTimer` block that drains the latest
  `UserStatus` and writes the title
* auto-reconnects with a 2 s backoff if the daemon stops
* fires threshold notifications via `osascript` when `remaining_seconds`
  crosses one of `warn_thresholds_minutes` (config-driven, shipped
  with each `UserStatus`)

All three communicate over a Unix socket at `/var/run/screentimed.sock`
(mode 0666). Wire types and framing live in the shared
`konstantin-proto` library — bumping a wire type means bumping `proto`.

## IPC: framing + peer creds

The socket lives at `/var/run/screentimed.sock` (configurable via
`socket_path`) with mode `0666` so any local user can connect. The
authentication is **peer credentials**, not the socket mode — on every
`accept` the daemon calls `getpeereid(2)` to learn the caller's real UID
and only ever returns or modifies state for that UID. The daemon **never**
trusts a UID sent over the wire.

Frame format on the socket:

```
+-----------------+----------------------+
|  u32 length BE  |  JSON payload bytes  |
+-----------------+----------------------+
```

One frame per request and per response. `MAX_FRAME_BYTES` is 1 MiB —
generous; real frames are < 1 KiB.

Two request lifecycles:

* **One-shot** (`GetStatus`, `ReportSessionState`) — read a frame, write
  a frame, loop.
* **Long-lived** (`Subscribe`) — write one immediate `StatusUpdate`, then
  push a fresh `StatusUpdate` every time the daemon's tick broadcast
  fires. Connection ends on either side closing.

## Counter model

The ticker fires every `tick_seconds`. On each tick:

1. Ask the system who's at the console via
   `SCDynamicStoreCopyConsoleUser` (a `SystemConfiguration` framework
   call). Returns 0 or 1 username — macOS has a single foreground
   console user at any moment. Empty at the login window or when no
   one is signed in.
2. For each active username, increment its counter by `tick_seconds`.
3. Atomically persist `state.json` (write `<path>.tmp`, then `rename(2)`).
4. Broadcast a wakeup to all `Subscribe` connections.
5. Run an enforcement pass.

This replaces an earlier `utmpx` walk. `utmpx` reported every
logged-in user (including ones backgrounded by Fast User Switching)
and lagged on graceful logout, so a user could continue accruing time
after they'd actually left.
`SCDynamicStoreCopyConsoleUser` reports only the FUS-foreground user
and flips immediately on logout.

`state.json` shape:

```json
{
  "day": "2026-04-26",
  "counters": { "alice": 1842, "bob": 0 }
}
```

`day` is the local calendar date these counters belong to. The set of
*currently-active* users is held only in memory (`#[serde(skip)]`) — a
stale state file on disk can't claim someone is logged in.

`SCDynamicStoreCopyConsoleUser` returns just the username; UIDs are
resolved lazily via `getpwnam` only when enforcement is about to act
on someone.

## Midnight reset

A separate task computes `next_local_midnight()` (via
`Local.from_local_datetime`, never `+86400 s` — that breaks on DST),
sleeps that duration, then calls `reset_if_new_day(today)`, persists,
broadcasts, and recomputes for the next day.

Defense in depth: the ticker also checks `reset_if_new_day` on each tick
and at startup, so a daemon that was suspended across midnight (where
the monotonic clock pauses but wall-clock advances) catches up
immediately on resume.

Ambiguous and nonexistent local midnights (rare, weird timezones) are
handled in `time::next_local_midnight_after`.

## SessionState lifecycle

`SessionState` (in `proto`) is what `compute_status` returns:

| State           | Meaning |
|-----------------|---------|
| `NotConfigured` | Username not in `[users.*]`. No limit applies. |
| `Active`        | Configured, has a console session, under limit, timer advancing. |
| `Offline`       | Configured but not currently logged in to a console session. |
| `LimitReached`  | Configured, over limit. `enforcement` decides what happens next. |
| `Paused`        | Wire-level only (proto carries the variant). The daemon never produces it — phase 8 was dropped, see CLAUDE.md decision #3. |

The transition `Active → LimitReached` happens silently inside
`compute_status`; the actual kick is decided by the enforcement pass.

## Enforcement

A two-step design lives in `crates/daemon/src/enforcement.rs`:

* **`decide(cfg, username, used, last_kicked, now) -> Decision`** is
  pure and unit-testable. Returns `NoOp` or
  `Kick(KickReason::{LimitReached, BlockedByDefaultPolicy})`.
* **`Enforcer::act_on`** performs the side effect.

In order:

1. **Backoff first** — if we acted on this UID less than 10 s ago,
   `decide` returns `NoOp`. Stops a tight kick → re-login → kick loop.
2. **Configured user over limit** → `Kick(LimitReached)`.
3. **Unconfigured user with `default_policy = "block"`** →
   `Kick(BlockedByDefaultPolicy)`.
4. **Otherwise** → `NoOp`.

The act phase:

1. Check `cfg.kill_switch_path.exists()` — if present, log a WARN and
   return without booting out, regardless of enforcement mode.
2. Resolve username → UID via `getpwnam`. If unresolvable, skip.
3. **`enforcement = "log"`** — log `would have kicked` and stamp
   `last_kicked`. Never spawns `launchctl`.
4. **`enforcement = "logout"`** — invoke `/bin/launchctl bootout
   user/<uid>` via `tokio::process::Command` with a 5 s timeout. On
   success, stamp `last_kicked`. On failure, log a WARN and *don't*
   stamp — we'll retry next tick.

`launchctl bootout` is best-effort: it can fail mid-transition,
especially on fast user switching. The retry-on-failure behavior is
intentional.

## Kill-switch

Filesystem touch-file (default `/etc/screentimed/disable`). Checked
before every actual bootout. Touch it to disable enforcement live;
remove it to re-enable. No daemon restart required, no config reload
needed.

The check happens in `Enforcer::act_on`, *after* `decide` returns a
`Kick`. So in Log mode, touching the kill-switch will replace
`would have kicked` log lines with `kill-switch present, refusing to
enforce` warnings. Both indicate enforcement decisions; the kill-switch
just intercepts the action.

The kill-switch path is *not* throttled by the backoff. If multiple
users are over their limit while the switch is active, you'll see one
warning per user per tick. This is intentional — a touched kill-switch
is meant to be highly visible.

## Push channel (`Subscribe`)

The daemon owns a single `tokio::sync::broadcast::Sender<()>`. Both the
ticker (after persist) and the midnight resetter (after reset, only when
the day actually rolled) call `tx.send(())`.

Each `Subscribe` connection holds its own `Receiver`, minted via
`tx.subscribe()`. The connection task does:

1. Send one immediate `StatusUpdate` (the proto contract).
2. Loop on `select!` between `rx.recv()` (broadcast wakeup → recompute
   status, send `StatusUpdate`) and `read_frame::<Request>(reader)` (for
   client-close detection).
3. `Lagged(n)` is recoverable — log + send a fresh snapshot, don't
   disconnect. The broadcast capacity is 64; lagging means a slow
   subscriber missed > 64 ticks.

Each subscriber's `compute_status` reads from the shared
`Arc<Mutex<State>>`, so the same status logic is used for one-shot
`GetStatus` and for pushed `StatusUpdate`s.

## Files & paths

| Path | Owner / mode | Purpose |
|------|--------------|---------|
| `/etc/screentimed/config.toml`        | root, 0600 | daemon config; readable only by root, so the tray's `Configure…` window prompts for admin auth before opening |
| `/etc/screentimed/disable`            | root | kill-switch (touch to disable enforcement) |
| `/var/db/screentimed/state.json`      | root, 0600 (parent dir 0700) | per-user counters, persists across restarts |
| `/var/run/screentimed.sock`           | root, 0666 | IPC socket; world-connectable, peer-creds-authenticated |
| `/Library/LaunchDaemons/com.gitopolis.screentimed.plist` | root | LaunchDaemon plist |
| `~/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist` | user | per-user LaunchAgent plist |

All five paths under `/etc` and `/var` are configurable; see
[config.md](config.md).

Uninstall (in-app `Uninstall…`, `packaging/uninstall.sh`, or
`brew uninstall konstantin`) wipes `/var/db/screentimed/` along with
the binaries and plists, but preserves `/etc/screentimed/` so a
reinstall picks up your settings. Use `brew uninstall --zap konstantin`
(or delete `/etc/screentimed/` by hand) for a full wipe.

## Threshold notifications

The daemon ships its `warn_thresholds_minutes` (e.g. `[15, 5, 1]`) inside
every `UserStatus`. The tray runs a small `NotifTracker` that watches
the stream:

* On day rollover (`resets_at` changes) — re-arm.
* Only fires while `state == Active`. `LimitReached` is silent (the
  user is being kicked, not warned).
* Picks the **smallest** threshold ≥ `remaining_seconds`. So a tray
  that subscribes late at 100 s remaining with thresholds `[15, 5, 1]`
  fires only the 5-minute warning, not also 15.
* Once fired, won't fire again until a strictly smaller threshold is
  crossed. So 15 → 5 → 1 each fire exactly once per day.

Dispatch uses `UNUserNotificationCenter` from the signed app bundle so
warnings are attributed to Konstantin and use the bundle icon. Developer
runs without an `NSBundle` context no-op with a tracing warning rather
than crashing.

## What's *not* tampered with

* No `pwpolicy` rules.
* No auth-plugin (`/Library/Security/SecurityAgentPlugins`) installation.
* No PAM modules.
* No Screen Recording, Accessibility, or Full Disk Access TCC prompts.
* No login-window injection.

The lockout method is "soft re-logout" via `launchctl bootout`. If a
kicked user logs back in, the next tick re-bootouts them (subject to
backoff). This is intentionally low-magic — you can disable enforcement
by touching the kill-switch, by editing `enforcement = "log"` in the
config, or by stopping the daemon. None of those leave persistent
artifacts on the system.
