# Project context for Claude

A macOS screen-time enforcer in Rust. This file is the design handoff —
read it before making changes so decisions stay consistent.

Owner: Nikita (qnicks@gmail.com). Target machine: macOS Tahoe on Apple
Silicon. Rust edition 2021, MSRV 1.78. Distribution intent: Homebrew
cask shipping an unsigned `Screentime.app` bundle. Developer ID
notarization is out of scope for v1.

## Architecture

Three Rust artifacts in one Cargo workspace, all eventually packaged
into a single `Screentime.app` bundle (see "App bundle architecture"
below).

* **`screentimed`** — privileged LaunchDaemon, runs as root. Owns
  config loading, per-user counters, console-session enumeration via
  `utmpx`, midnight resets, the `Subscribe` push channel, and forced
  logouts via `launchctl bootout user/<uid>`.
* **`screentime-tray`** — per-user LaunchAgent (Aqua only). `NSStatusBar`
  / `NSStatusItem` menu-bar app subscribed to the daemon's push channel.
  Shows remaining time live, fires threshold notifications, and (in
  progress) lets the operator start / stop / restart / configure the
  daemon via menu items gated by a system admin-password prompt.
* **`screentime-proto`** — shared library: wire types (`Request`,
  `Response`, `UserStatus`, `SessionState`) and length-prefixed JSON
  framing helpers.

A headless `screentime-status` CLI also lives in the tray crate. Useful
for diagnostics; not user-facing once the `.app` ships.

IPC is over a Unix socket at `/var/run/screentimed.sock` (mode 0666).
Authentication is via `nix::unistd::getpeereid` (returned to the safe
nix wrapper after the 0.31 upgrade — the daemon no longer calls libc
on this path). The daemon **never** trusts UIDs sent over the wire and
only ever returns or modifies state for the peer UID. The tray is
intentionally dumb: all policy decisions (counting, kicking) live in
the daemon so a user can't tamper with their own limit.

Frame format: `[u32 length BE][JSON payload]`. One frame per request
and per response. `Subscribe` keeps the connection open and the daemon
pushes `StatusUpdate` frames every tick (and on midnight rollover).
See `docs/concepts.md` for the IPC narrative.

## App bundle architecture (in progress)

The end-user product is one `.app` bundle, distributed via Homebrew
cask. Bundle layout:

```
Screentime.app/Contents/
  Info.plist                                — LSUIElement=true (no Dock icon)
  MacOS/screentime-tray                     — main bundle executable
  Library/LaunchDaemons/com.qnicks.screentimed.plist
  Resources/
    screentimed                             — daemon binary (copied to
                                              /usr/local/libexec/ at install)
    screentime-status                       — diagnostic CLI
    config.example.toml
    AppIcon.icns
```

First-launch flow:

1. Tray attempts `UnixStream::connect("/var/run/screentimed.sock")`.
   If it succeeds, the daemon is already running — proceed with normal
   subscribe.
2. If the connect fails AND `/Library/LaunchDaemons/com.qnicks.screentimed.plist`
   is missing, the tray puts up a "Set up Screentime" alert that runs
   the install steps (copy daemon binary into `/usr/local/libexec/`,
   plist into `/Library/LaunchDaemons/`, `launchctl bootstrap`).
3. The per-user LaunchAgent is dropped into `~/Library/LaunchAgents/`
   (no auth needed for that one).

Privileged menu actions (Start / Stop / Restart / Configure /
Uninstall) all funnel through one `run_as_root` helper that wraps
`osascript -e 'do shell script "..." with administrator privileges'`.
macOS shows the standard password sheet; the command then runs as
root. Each invocation is one-shot — no persistent privilege handle.

Daemon-running detection: try to connect to the socket. Cheap, no
privileges, no `launchctl print` parsing. The worker thread's existing
`disconnected` flag drives both the menu-item enabled state and the
red-dot status indicator.

## Decisions locked in

These were Nikita's choices; revisit explicitly before changing them.

1. **Lockout method: soft re-logout.** When a user hits zero, the
   daemon calls `launchctl bootout user/<uid>`. If they log back in,
   the next session-poll tick re-bootouts them. No `pwpolicy` or
   auth-plugin tampering. 10-second per-uid recently-kicked backoff
   prevents tight loops.

2. **Time accounting v1: logged-in time.** Daemon parses `utmpx` and
   increments per-user counters every tick. No idle / lock detection
   in v1. Console sessions only (filter `ut_line` starting with
   `console`); SSH and tty are skipped.

3. **Time accounting stays v1 — phase 8 is dropped.** We considered
   pausing counters on user-reported lock / idle (the proto already
   carries `Request::ReportSessionState { locked, idle_seconds }`),
   but it's not implementable as a real boundary: lock/idle are
   session-bound APIs (`CGSessionCopyCurrentDictionary`,
   `CGEventSourceSecondsSinceLastEventType`) that a root daemon
   *outside* any Aqua session can't authoritatively query. Trusting
   the tray's report makes it trivially fakeable — a 5-line script
   sending `locked: true` every few seconds would pause the counter
   forever, and the 30 s sanity-check only catches "user killed their
   tray," not "user sends fake reports." Logged-in time is what we
   have; it's adversarially robust by accident (utmpx is daemon-owned)
   and "be at the keyboard for 2 hours" is arguably the limit you
   actually want. The wire type stays in `proto` as dead surface — the
   daemon `Ack`s and ignores it. Remove it on the next breaking proto
   bump if it bothers you.

4. **Reset: local midnight.** Compute `next_local_midnight()` by
   recomputing via `Local.from_local_datetime` after each reset —
   never add 86400 s, that drifts on DST. Startup also calls
   `reset_if_new_day` in case the daemon was off across midnight.

5. **Warnings: threshold notifications.** Tray fires macOS notifications
   at the configured `warn_thresholds_minutes` (default `[10, 2, 1]`).
   Only the smallest applicable threshold fires on any given crossing
   (a tray that joins late at 100 s remaining fires the 5 m / 2 m
   warning, not 15 m / 10 m). Re-arm on day rollover detected via
   `UserStatus::resets_at` change.

6. **Notifications via `osascript`** (not `UNUserNotificationCenter`).
   `osascript` is signed by Apple, so notifications work without TCC
   consent or bundle signing. Trade: notifications attribute to
   "Script Editor" rather than "Screentime". `UNUserNotificationCenter`
   is the future path once Developer ID signing is in place.

7. **`.app` bundle is the canonical install path.** The bundle ships
   the daemon binary at `Contents/Resources/screentimed`. On
   "Set up Screentime", the binary is **copied** (not symlinked) into
   `/usr/local/libexec/`. Re-running setup re-copies, fixing version
   skew when the user upgrades the bundle.

8. **Privilege model: `osascript` admin auth.** All privileged actions
   funnel through one `run_as_root` helper; no persistent privilege
   handle, no `SMJobBless` helper, no `SMAppService`. SMAppService is
   the right Mac path long-term but requires Developer ID signing,
   which we don't have yet.

9. **Menu actions are system-wide** (like Docker Desktop). Start / Stop
   / Restart affect the one shared daemon; Configure edits
   `/etc/screentimed/config.toml`. There is no per-user mode.

10. **Status item: red dot when daemon is down**, formatted remaining
    time when it's up. Detection is the worker's `disconnected` flag,
    driven by socket-connect success.

## What's already built (phases 1–7)

* **Phase 1** — proto + framing + peer-creds auth, `GetStatus` returns
  real status from current state.
* **Phase 2** — `utmpx` walk, per-user counters, atomic `state.json`
  (write `<path>.tmp` + `rename(2)`), in-memory `active_now` set.
* **Phase 3** — DST-correct midnight reset task with opportunistic
  per-tick check as defense in depth.
* **Phase 4** — `Subscribe` push channel via
  `tokio::sync::broadcast::channel<()>`. Immediate snapshot on connect,
  `Lagged(n)` recovers with a fresh snapshot, no disconnect.
* **Phase 5** — enforcement: pure `decide()` function;
  `Enforcer::act_on` with kill-switch (`kill_switch_path`, default
  `/etc/screentimed/disable`) and 10 s per-uid backoff. `enforcement
  = "log"` is the default; `"logout"` invokes `/bin/launchctl bootout
  user/<uid>` with a 5 s timeout and best-effort retry on failure.
  **Live verification of Logout mode against `alice`/`bob` is still
  pending.**
* **Phase 6** — `screentime-tray` binary using `objc2 = "0.6"`,
  `objc2-app-kit = "0.3"`, `objc2-foundation = "0.3"`, `block2 = "0.6"`.
  Main thread runs `NSApplication` and a 5 Hz `NSTimer` block; worker
  thread hosts a current-thread tokio runtime running the
  `Subscription` loop with auto-reconnect (2 s backoff).
* **Phase 7** — threshold notifications via `osascript`. Pure
  `NotifTracker` decision logic; tracker resets on `resets_at` change,
  fires the smallest applicable threshold only, suppresses
  `LimitReached` (user is being kicked, not warned).

Tests: 21 passing at last commit (proto×2, daemon×13, tray×6). Unsafe
block count: 4 (one selector cast, one `NSTimer` block ABI, two
`utmpx` FFI walks). Both numbers will drift — they're a "currently
healthy" marker, not a target.

## What's NOT built yet — the next push

One effort: bundle the workspace into a single `Screentime.app` with
an admin-gated menu. Phase 8 (lock/idle) was dropped — see decision #3.

### A. App bundle + admin-gated menu (the active work)

| Phase | Scope |
|-------|-------|
| A1    | Build `Screentime.app/` from `target/release/`. New `packaging/build-app.sh`. `Info.plist` with `LSUIElement=true`, AppIcon.icns. |
| A2    | First-launch install. Tray detects "no daemon" via socket connect; if it fails AND no system-side plist, prompts for admin and runs install via `osascript`. Drops the per-user LaunchAgent into `~/Library/LaunchAgents/` (no auth). |
| A3    | `run_as_root` primitive in the tray crate (`osascript`-backed). Returns `Cancelled` / `Failed` / `Ok`; surfaces alerts on failure. |
| A4    | Menu items: Start / Stop / Restart wrap `launchctl bootstrap` / `bootout` / `kickstart -k system/com.qnicks.screentimed`. Configure: copy `/etc/screentimed/config.toml` to `$TMPDIR`, `open -t`, FSEvents-watch for save, then `run_as_root` cp + kickstart. Open Log: `open /var/log/screentimed.log`. |
| A5    | State-driven UI: 🔴 when `disconnected`, time string otherwise. Menu items enable/disable based on the same flag. |
| A6    | Bundle-aware paths. Daemon, plist, config-example resolved relative to `Contents/Resources/`. |
| A7    | Uninstall flow. `Uninstall…` menu item runs `run_as_root` against the equivalent of `uninstall.sh`. Cask `zap` block for non-interactive `brew uninstall --zap`. |

## Hard safety rules

These exist because the daemon can lock the operator out of their own
machine.

* **Never test on existing user accounts.** Always use the `alice` /
  `bob` test accounts created by `packaging/create-test-users.sh`.
  UIDs 601 / 602; passwords `screentime-test-alice` /
  `screentime-test-bob`.
* **Keep `enforcement = "log"`** until you have manually verified
  `enforcement = "logout"` against alice or bob. The decision logic is
  unit-tested but the live `launchctl bootout` invocation is not.
* **Keep `default_policy = "unrestricted"`.** Switching to `"block"`
  before every account is enumerated will kick everyone, including
  admins.
* **Never put the operator's account in `[users.*]`** unless you mean
  it. The example config only lists alice / bob for this reason.
* **Kill-switch is `/etc/screentimed/disable`** by default. Touch the
  file to suppress all enforcement actions live; remove it to
  re-enable. Configurable via `kill_switch_path` in TOML.

## Build, smoke-test, install (development workflow)

End users will install via Homebrew cask once A1 ships. For development,
the existing scripts remain authoritative.

```sh
cargo check --workspace
cargo test  --workspace
cargo build --release
```

Smoke-test without installing (no root). Use an ad-hoc config (e.g.
at `/tmp/screentimed-smoketest.toml`, gitignored) — see
`docs/concepts.md` for the full recipe.

System install (requires root):

```sh
sudo packaging/create-test-users.sh        # makes alice + bob
cargo build --release
sudo packaging/install.sh                  # daemon + tray for $SUDO_USER
sudo packaging/install.sh alice bob        # also/instead: tray for those users
```

Per-user-only tray install (no root):

```sh
./packaging/install-tray.sh
```

Uninstall:

```sh
sudo packaging/uninstall.sh                # daemon + binaries + tray for $SUDO_USER
./packaging/uninstall-tray.sh              # tray-only, no root
```

`uninstall.sh` removes binaries / plists / socket but preserves
`/etc/screentimed/` and `/var/db/screentimed/` so configs and counters
survive reinstalls.

## Layout

```
screentime/
├── Cargo.toml                                          # workspace root
├── CLAUDE.md                                           # this file
├── README.md
├── docs/
│   ├── concepts.md                                     # architecture deep-dive
│   ├── config.md                                       # config schema reference
│   └── cli.md                                          # binary flags + env vars
├── crates/
│   ├── proto/      src/lib.rs                          # types + framing
│   ├── daemon/     src/{main,config,ipc,sessions,
│   │                   state,time,enforcement}.rs      # screentimed
│   └── tray/       src/{lib,notifications}.rs,
│                   src/bin/{status,tray}.rs            # screentime-status + tray
└── packaging/
    ├── com.qnicks.screentimed.plist            # LaunchDaemon, runs as root
    ├── com.qnicks.screentime-tray.plist        # LaunchAgent, Aqua-only
    ├── config.example.toml
    ├── install.sh, uninstall.sh                # system-side, requires sudo
    ├── install-tray.sh, uninstall-tray.sh      # per-user, no sudo
    └── create-test-users.sh, delete-test-users.sh
```

## macOS Tahoe gotchas

* **Code signing.** Distribution via Homebrew cask without Developer
  ID is acceptable — cask handles `com.apple.quarantine` removal.
  SMAppService and `UNUserNotificationCenter` both require Developer
  ID; they're deferred until signing is in place.
* **Modern install path.** For real distribution prefer
  `SMAppService.daemon(plistName:)` over hand-placed
  `/Library/LaunchDaemons/` files. We use the hand-placed approach
  via `run_as_root` because it works without code signing.
* **Fast user switching.** `utmpx` will show every logged-in user;
  counters advance per-user in parallel. State file already keys by
  username for this reason.
* **`launchctl bootout` is best-effort.** It can fail mid-transition,
  especially under fast user switching. Treat as best-effort, log
  failures, retry next tick (already the behavior in
  `enforcement::Enforcer::act_on`).

## Style notes

* Logging via `tracing`. Structured fields, not `println!`.
* Errors: `anyhow` in binaries, `thiserror` in libraries.
* Async I/O: tokio. Keep the daemon single-runtime.
* Don't introduce per-user state outside `state.json`.
* Don't add new wire types without bumping the docs in
  `crates/proto/src/lib.rs` — clients deserialize against them.
* Workspace-pinned majors: `objc2 = "0.6"`, `objc2-app-kit = "0.3"`,
  `objc2-foundation = "0.3"`, `block2 = "0.6"`, `nix = "0.31"`,
  `thiserror = "2"`, `toml = "1"`. Bump deliberately, not via
  `cargo update`.
