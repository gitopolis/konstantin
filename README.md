# screentime

A macOS screen-time enforcer written in Rust.

* `screentimed` — privileged LaunchDaemon. Reads config, enumerates console
  sessions via `utmpx`, tracks per-user used-seconds, persists state to
  disk, resets at local midnight, and (when configured to) kicks users
  over their daily limit via `launchctl bootout`.
* `screentime-status` — headless CLI client. Asks the daemon "what's my
  status?" over a Unix socket. Supports `--watch` for a live stream.
* `screentime-tray` — per-user menu-bar app. Library exists; UI binary
  lands in phase 6.

## Documentation

* [docs/concepts.md](docs/concepts.md) — architecture, counters,
  enforcement, IPC, midnight reset, kill-switch.
* [docs/config.md](docs/config.md) — every config field, with defaults
  and behavior.
* [docs/cli.md](docs/cli.md) — binary flags, environment variables, exit
  codes.

## Layout

```
screentime/
├── Cargo.toml              # workspace root
├── CLAUDE.md               # design notes (handoff to future contributors)
├── crates/
│   ├── proto/              # serde wire types + length-prefixed framing
│   ├── daemon/             # `screentimed` binary
│   └── tray/               # `screentime-status` (and later `screentime-tray`)
├── docs/                   # user-facing reference
└── packaging/
    ├── com.qnicks.screentimed.plist        # LaunchDaemon
    ├── com.qnicks.screentime-tray.plist    # per-user LaunchAgent
    ├── config.example.toml
    ├── install.sh
    ├── uninstall.sh
    ├── create-test-users.sh                # makes alice + bob
    └── delete-test-users.sh
```

## Build

```sh
cargo build --release
cargo test  --workspace
```

## Smoke-test (no install required)

The example config writes to root-owned paths. For a local test, copy it
to a writable location and edit the paths:

```sh
mkdir -p ./run
cp packaging/config.example.toml /tmp/screentimed.toml
# edit /tmp/screentimed.toml: set
#   socket_path = "./run/screentimed.sock"
#   state_path  = "./run/state.json"
```

In one terminal:

```sh
SCREENTIMED_CONFIG=/tmp/screentimed.toml \
SCREENTIMED_LOG=debug \
./target/release/screentimed
```

In another terminal — single status query:

```sh
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/screentime-status
```

For the operator's account (not in `[users.*]`) you'll get
`state: NotConfigured`. If you add yourself to the config with a sane
`daily_limit_minutes`, you'll see `state: Active` and a counter that
advances every `tick_seconds`.

Live-stream the same view:

```sh
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/screentime-status --watch
```

## Install on this machine

```sh
sudo ./packaging/create-test-users.sh   # creates alice (uid 601), bob (uid 602)
cargo build --release
sudo ./packaging/install.sh             # installs binaries + plists, starts daemon
```

The example config has `enforcement = "log"`, so the daemon writes
`would have kicked X` to its log but never actually invokes
`launchctl bootout`. Keep it in `"log"` mode while developing.

To disable enforcement live without restarting, touch the kill-switch
file (default `/etc/screentimed/disable`); remove it to re-enable.

To remove everything:

```sh
sudo ./packaging/uninstall.sh
sudo ./packaging/delete-test-users.sh
```

## Safety

The daemon can lock the operator out of their own machine. Read
`docs/concepts.md` and the "Hard safety rules" section of `CLAUDE.md`
before changing the enforcement path. In short:

* Always test against `alice` / `bob`, never on existing user accounts.
* Keep `enforcement = "log"` until phase 5 has been manually verified
  on a test account.
* Keep `default_policy = "unrestricted"` unless every account that
  should be allowed in has been added to `[users.*]`.
* Don't put the operator's account in `[users.*]` unless you mean it.

## Roadmap

| Phase | Status      | What lands |
|-------|-------------|------------|
| 1     | ✅ done     | proto + framing + peer-creds-authenticated stub IPC |
| 2     | ✅ done     | utmpx session enumeration, per-user counters, JSON state file |
| 3     | ✅ done     | midnight reset task (DST-correct) |
| 4     | ✅ done     | `Subscribe` push channel for live tray updates |
| 5     | ✅ done     | forced logout via `launchctl bootout` (gated by `enforcement = "logout"` + kill-switch + backoff) |
| 6     | next        | menu-bar UI binary (`screentime-tray`) using `objc2` + `NSStatusItem` |
| 7     |             | threshold notifications (15 / 5 / 1 minutes remaining) |
| 8     |             | (v2) per-user lock/idle reporting via the tray, with a 30 s sanity-check fallback |
