# CLI reference

Both binaries take their inputs from environment variables and command-line
flags. Neither reads stdin.

## `screentimed`

The privileged daemon. Normally run as a LaunchDaemon by `launchd`; for
development you run it directly (or under `cargo run`).

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SCREENTIMED_CONFIG` | `/etc/screentimed/config.toml` | Path to the TOML config. See [config.md](config.md) for the schema. |
| `SCREENTIMED_LOG`    | `info,screentimed=debug,konstantin_proto=info` | `tracing` `EnvFilter` directive. Standard syntax: `info`, `debug`, `screentimed::ipc=trace`, etc. |

### Flags

The daemon currently takes no command-line flags. All knobs live in the
config file and the env vars above. If you want to test against a
different socket / state path / enforcement mode, edit a TOML file and
point `SCREENTIMED_CONFIG` at it.

### Signals

| Signal  | Effect |
|---------|--------|
| `SIGTERM` | Graceful shutdown. The IPC socket is removed via the `Drop` impl on `ipc::Server`. |
| `SIGINT`  | Same as `SIGTERM`. Use `Ctrl-C` from the terminal. |

`state.json` is preserved across shutdown — counters resume on next start
unless the `day` field is older than today (in which case they're zeroed).

### Exit codes

The daemon does not use distinct exit codes. Any unhandled `Result::Err`
from `tokio::main` produces exit code 1.

### Logging

Structured `tracing` output. Default format is the `tracing-subscriber`
text format: timestamp, level, target, key=value fields. Levels of
interest:

* `INFO screentimed: config loaded …`
* `INFO screentimed::ipc: listening …`
* `INFO screentimed: midnight resetter sleeping target=… wait_s=…`
* `INFO screentimed: counters reset for new day …`
* `INFO screentimed::enforcement: would have kicked … reason=LimitReached`
* `INFO screentimed::enforcement: kicking via launchctl bootout`
* `WARN screentimed::enforcement: kill-switch present, refusing to enforce`
* `WARN screentimed::enforcement: bootout failed (will retry next tick)`
* `DEBUG screentimed::ipc: client connected uid=… username=…`
* `DEBUG screentimed::ipc: client subscribed …`

To get just enforcement decisions:

```sh
SCREENTIMED_LOG=screentimed::enforcement=info ./screentimed
```

## `konstantin-status`

The headless CLI client. Useful before the menu-bar UI exists, and as a
diagnostic afterwards.

### Usage

```
konstantin-status                  # one-shot, human-readable
konstantin-status --json           # one-shot, pretty JSON
konstantin-status --watch          # subscribe, stream compact lines
konstantin-status -w               # short form of --watch
konstantin-status --watch --json   # subscribe, one JSON object per line
```

### Flags

| Flag         | Effect |
|--------------|--------|
| `--json`     | Emit machine-readable JSON instead of the human block. In one-shot mode: pretty-printed once. In `--watch` mode: one compact JSON object per line, suitable for `jq` / pipes. |
| `--watch`, `-w` | Subscribe to the daemon and print each pushed `StatusUpdate`. The daemon pushes one frame on subscribe, then one per `tick_seconds`, then one on midnight rollover. |

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SCREENTIMED_SOCKET` | `/var/run/screentimed.sock` | Path to the daemon's IPC socket. |

### Output formats

One-shot human:

```
user      : nikita (uid 501)
state     : Active
daily     : 24h00m
used      : 1m04s
remaining : 23h58m
resets_at : 2026-04-27 00:00:00 -0700
```

`--watch` compact:

```
[14:26:20] nikita state=Active used=1m04s remaining=23h58m
[14:26:25] nikita state=Active used=1m09s remaining=23h58m
```

`--watch --json`:

```json
{"uid":501,"username":"nikita","state":"active","daily_limit_seconds":86400,"used_seconds":64,"remaining_seconds":86336,"resets_at":"2026-04-27T00:00:00-07:00"}
```

### Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Success. In `--watch` mode, the daemon closed the connection cleanly. |
| `2`  | Transport / decode error (cannot connect, frame malformed, daemon vanished). |
| `3`  | Daemon-side error response (currently unused — the daemon does not return `Response::Error` for either `GetStatus` or `Subscribe` after phase 4). |

## `konstantin-tray`

The per-user menu-bar app. macOS only. Normally run as a LaunchAgent
in Aqua sessions; for development you can launch it directly from a
GUI terminal session.

### Usage

No arguments. Show or hide via the menu-bar item; quit via the menu's
"Quit" option (`⌘Q`).

### Admin menu actions

Routine operator actions use the signed admin XPC channel exposed by
the root daemon. Standard users can see their own status, but only
local admins can configure, pause/unpause enforcement, reload, update,
or uninstall.

Notable actions:

* `Configure…` reads and writes `/etc/screentimed/config.toml` through
  the daemon, preserving mode 0600 root ownership.
* `Pause Enforcement` / `Unpause Enforcement` creates or removes the
  configured kill-switch file (default `/etc/screentimed/disable`).
* `Check for Updates…` downloads and verifies the release zip in the
  tray, then asks the daemon to stage and launch the detached
  `konstantin-updater` helper. The helper swaps the bundle, restarts
  the daemon, and rolls back if the new daemon does not become reachable.
* `Uninstall…` removes the binaries, plists, socket, and counter state
  at `/var/db/screentimed/`. It preserves `/etc/screentimed/` so a
  reinstall picks up your settings.

First-launch setup registers the bundled daemon through
ServiceManagement. Development builds should be packaged with
`./packaging/build-app.sh` and launched from `target/Konstantin.app`
instead of using the tray to install a dev-tree daemon.

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SCREENTIMED_SOCKET`    | `/var/run/screentimed.sock` | Path to the daemon's IPC socket. |
| `KONSTANTIN_TRAY_LOG`   | `info,konstantin_tray=info` | `tracing` `EnvFilter` directive. |

### Behavior

* On startup the title is `🔴` until the first `StatusUpdate` arrives.
* While connected the title reflects `remaining_seconds`:
  * `Active` → `format_remaining(remaining_seconds)` (e.g. `1h23m`,
    `12m05s`)
  * `LimitReached` → `0s`
  * `Offline` → `offline`
  * `NotConfigured` → `—`
* When the daemon stops the title becomes `🔴` and the worker retries
  `Subscription::open` every 2 s. Reconnect is automatic; no user
  action needed.
* When `remaining_seconds` crosses one of `warn_thresholds_minutes`
  (configured daemon-side, shipped with each `UserStatus`), the tray
  fires a Notification Center warning via `UNUserNotificationCenter`.
  Each threshold fires at most once per day; the smallest applicable
  threshold wins on first crossing.

### Logs

When run as a LaunchAgent, stdout/stderr go to
`~/Library/Logs/konstantin-tray.out.log` and
`~/Library/Logs/konstantin-tray.err.log` for the target user.

## Building

```sh
cargo build --release
```

Binaries land in `target/release/`. The release profile uses thin LTO
and stripped symbols (see workspace `Cargo.toml`).

## Running tests

```sh
cargo test --workspace
```

Covers proto framing roundtrips, state save/load, console-user
enumeration shape, midnight computation, and the enforcement decision
matrix. The `launchctl bootout` invocation itself is not unit-tested
— verify it manually against an `alice` / `bob` test account at
install time.
