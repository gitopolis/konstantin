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
| `SCREENTIMED_LOG`    | `info,screentimed=debug,screentime_proto=info` | `tracing` `EnvFilter` directive. Standard syntax: `info`, `debug`, `screentimed::ipc=trace`, etc. |

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

## `screentime-status`

The headless CLI client. Useful before the menu-bar UI exists, and as a
diagnostic afterwards.

### Usage

```
screentime-status                  # one-shot, human-readable
screentime-status --json           # one-shot, pretty JSON
screentime-status --watch          # subscribe, stream compact lines
screentime-status -w               # short form of --watch
screentime-status --watch --json   # subscribe, one JSON object per line
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

Covers proto framing roundtrips, state save/load, utmpx field parsing,
midnight computation, and the enforcement decision matrix. The
`launchctl bootout` invocation itself is not unit-tested — verify it
manually against an `alice` / `bob` test account at install time.
