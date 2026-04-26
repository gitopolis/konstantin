# Project context for Claude

A macOS screen-time enforcer in Rust. This file is a handoff from the
planning conversation that produced the current scaffold; read it before
making changes so design decisions stay consistent.

Owner: Nikita (qnicks@gmail.com). Target machine: macOS Tahoe on Apple
Silicon. Rust edition 2021, MSRV 1.78.

## Architecture

Three Rust artifacts in one Cargo workspace:

* **`screentimed`** — privileged LaunchDaemon, runs as root, owns config,
  per-user counters, session enumeration, midnight resets, and forced
  logouts.
* **`screentime-tray`** — per-user LaunchAgent (Aqua-only). Menu-bar UI
  showing remaining time and threshold notifications. Currently exists only
  as `screentime-status`, a headless CLI client; the `NSStatusItem` UI is
  TODO (phase 6).
* **`screentime-proto`** — shared library: `Request` / `Response` /
  `UserStatus` / `SessionState` serde types and length-prefixed JSON
  framing helpers.

IPC is over a Unix socket at `/var/run/screentimed.sock` (mode 0666).
Authentication is via `getpeereid(2)` — the daemon **never** trusts UIDs
sent over the wire and only ever returns or modifies state for the peer
UID. The tray is intentionally dumb: all policy decisions (counting,
kicking) live in the daemon so a user can't tamper with their own limit.

Frame format: `[u32 length BE][JSON payload]`. One frame per request and
per response. `Subscribe` keeps the connection open and the daemon pushes
`StatusUpdate` frames on every change (currently stubbed — see roadmap).

## Decisions locked in during planning

These were Nikita's choices; revisit explicitly before changing them.

1. **Lockout method: soft re-logout.** When a user hits zero, the daemon
   calls `launchctl bootout user/<uid>`. If they log back in, the next
   session-poll tick re-bootouts them. No `pwpolicy` or auth-plugin
   tampering. Add a short "recently kicked" backoff (~10 s) so we don't
   spam-bootout in a tight loop.

2. **Time accounting v1: logged-in time.** Easiest to implement — daemon
   parses `utmpx` and increments per-user counters every tick. No idle or
   lock detection needed. Counts only console sessions (filter
   `ut_line` starting with `console`); SSH and tty are skipped.

3. **Time accounting v2 (later): active use.** The protocol already has
   `Request::ReportSessionState { locked, idle_seconds }` for the tray to
   report up. Daemon will pause counters on lock or idle. Sanity check:
   if no report arrives for 30 s, fall back to logged-in counting so a
   user can't pause their timer by killing their tray.

4. **Reset: local midnight.** Compute `next_midnight_local()` after each
   reset — don't add 86400 seconds, that breaks on DST.

5. **Warnings: threshold notifications.** The tray shows macOS
   notifications at 15/5/1 minutes remaining. Tray-side, not daemon-side
   (the daemon only reports remaining time). Default thresholds in
   `config.example.toml`; config-driven.

## What's already built (phase 1, in this scaffold)

* Workspace + crates compile-ready (verify with `cargo check`).
* `screentime-proto`: types + framing + roundtrip unit tests.
* `screentimed`:
  * loads `/etc/screentimed/config.toml` (path overridable via
    `SCREENTIMED_CONFIG`)
  * binds Unix socket, chmods 0666, removes stale socket on restart
  * authenticates clients via `getpeereid` and resolves username via
    `nix::unistd::User::from_uid`
  * answers `GetStatus` with **stub** `used_seconds: 0` and a real
    `resets_at` from `next_local_midnight()`
  * `Subscribe` returns an explicit "not implemented" error (don't change
    that until phase 4 lands)
  * graceful SIGTERM / SIGINT shutdown, removes socket on drop
  * structured logging via `tracing` (`SCREENTIMED_LOG` env var)
* `screentime-tray` crate ships `screentime-status` (headless CLI). Library
  exposes `fetch_status`, `default_socket_path`, `format_remaining` for
  the future menu-bar binary.
* `packaging/`: LaunchDaemon + LaunchAgent plists, example config,
  `install.sh`, `uninstall.sh`, `create-test-users.sh` (alice UID 601 /
  bob UID 602), `delete-test-users.sh`.

## What's NOT built yet — implement in this order

| Phase | Scope |
|-------|-------|
| 2     | utmpx session enumeration, per-user counters, JSON state file at `/var/db/screentimed/state.json` (atomic write: tmp + rename) |
| 3     | midnight reset task: `tokio::time::sleep_until(next_midnight)` + recompute after each reset |
| 4     | `Subscribe` push channel: server pushes `StatusUpdate` every tick and on transitions; tray uses this for live countdown |
| 5     | forced logout via `launchctl bootout` — gated by `enforcement = "logout"` config; default stays `"log"` |
| 6     | menu-bar UI binary in `crates/tray/src/bin/tray.rs` using `objc2` + `objc2-app-kit` (`NSStatusBar` / `NSStatusItem`) — prefer this over the deprecated `cocoa`/`objc` crates |
| 7     | threshold notifications via `mac-notification-sys` or `UNUserNotificationCenter` |
| 8     | (v2) tray reports lock/idle, daemon pauses counters with the 30 s sanity-check fallback |

## Hard safety rules

These exist because the daemon can lock the operator out of their own machine.

* **Never test on existing user accounts.** Always use the `alice` / `bob`
  test accounts created by `packaging/create-test-users.sh`. They have
  UIDs 601 / 602 and passwords `screentime-test-alice` /
  `screentime-test-bob`.
* **Keep `enforcement = "log"` in the config until phase 5 has been
  manually verified.** In log mode the daemon writes "would have kicked X"
  but never actually invokes `launchctl bootout`.
* **Keep `default_policy = "unrestricted"`.** Switching to `"block"`
  before every account is enumerated will kick everyone, including admins.
* **Never put the operator's account (`nikita`) in the `[users.*]`
  section** unless you mean it. The example config only lists alice/bob
  for this reason.
* When wiring up phase 5, add a kill-switch: an env var or a
  filesystem-touch-file (e.g. `/etc/screentimed/disable`) that the daemon
  checks before every bootout. Easier than rebuilding when a bug shows up.

## Build, smoke-test, install

Build:

```sh
cargo check --workspace
cargo test  -p screentime-proto
cargo build --release
```

Smoke-test without installing (no root needed). First edit
`packaging/config.example.toml` to set
`socket_path = "./run/screentimed.sock"`. Then in two terminals:

```sh
# terminal 1
mkdir -p run
SCREENTIMED_CONFIG=./packaging/config.example.toml \
SCREENTIMED_LOG=debug \
./target/release/screentimed
```

```sh
# terminal 2
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/screentime-status
```

For the operator's account this should print `state: NotConfigured`,
`daily: 0s`, `resets_at:` next local midnight. If it does, framing,
peer-creds auth, and config loading are all working end-to-end.

Install on the real machine:

```sh
sudo packaging/create-test-users.sh
cargo build --release
sudo packaging/install.sh
```

`uninstall.sh` removes binaries / plists / socket but preserves
`/etc/screentimed/` and `/var/db/screentimed/`.

## Layout

```
screentime/
├── Cargo.toml                                      # workspace root
├── CLAUDE.md                                       # this file
├── README.md
├── crates/
│   ├── proto/      src/lib.rs                      # types + framing
│   ├── daemon/     src/{main,config,ipc}.rs        # screentimed
│   └── tray/       src/lib.rs, src/bin/status.rs   # screentime-status
└── packaging/
    ├── com.qnicks.screentimed.plist        # LaunchDaemon, runs as root
    ├── com.qnicks.screentime-tray.plist    # LaunchAgent, Aqua-only
    ├── config.example.toml
    ├── install.sh, uninstall.sh
    └── create-test-users.sh, delete-test-users.sh
```

There may also be an orphan `src/main.rs` and an old `Cargo.lock` at the
repo root — leftover from before the workspace conversion, which couldn't
be removed from the planning sandbox. Safe to `rm -rf src/ Cargo.lock` on
your machine; Cargo will regenerate the lockfile.

## macOS Tahoe gotchas

* **No TCC prompts needed for v1.** This design avoids Screen Recording
  and Accessibility. The tray will need Notifications consent in phase 7
  (`UNUserNotificationCenter`). If the bundle is unsigned, fall back to
  `osascript -e 'display notification ...'`.
* **Code signing.** Ad-hoc (`codesign -s -`) is fine for personal use; for
  distribution you need Developer ID + notarization. macOS Tahoe continues
  Apple's tightening trend — don't ship unsigned.
* **Modern install path.** For real distribution prefer
  `SMAppService.daemon(plistName:)` over hand-placed
  `/Library/LaunchDaemons/` files. The current `install.sh` uses the
  hand-placed approach because it's simpler for development.
* **Fast user switching.** `utmpx` will show every logged-in user; counters
  must advance per-user in parallel. The state file format already keys by
  username for this reason.
* **`launchctl bootout` is best-effort.** It can fail mid-transition.
  Treat the call as best-effort, log failures, retry next tick.

## Style notes

* Logging via `tracing`. Structured fields, not `println!`.
* Errors: `anyhow` in binaries, `thiserror` in libraries. The pattern is
  already in place — follow it.
* Async I/O: tokio. Keep the daemon single-runtime.
* Don't introduce per-user state outside the `state.json` file.
* Don't add new wire types without bumping the docs in
  `crates/proto/src/lib.rs` — clients will deserialize against them.
