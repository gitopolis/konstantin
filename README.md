# konstantin

A macOS screen-time enforcer written in Rust.

* `screentimed` — privileged LaunchDaemon. Reads config, enumerates console
  sessions via `utmpx`, tracks per-user used-seconds, persists state to
  disk, resets at local midnight, and (when configured to) kicks users
  over their daily limit via `launchctl bootout`.
* `konstantin-status` — headless CLI client. Asks the daemon "what's my
  status?" over a Unix socket. Supports `--watch` for a live stream.
* `konstantin-tray` — per-user menu-bar app. Subscribes to the daemon's
  push channel and shows remaining time live in `NSStatusItem`.

## Documentation

* [docs/concepts.md](docs/concepts.md) — architecture, counters,
  enforcement, IPC, midnight reset, kill-switch.
* [docs/config.md](docs/config.md) — every config field, with defaults
  and behavior.
* [docs/cli.md](docs/cli.md) — binary flags, environment variables, exit
  codes.

## Layout

```
konstantin/
├── Cargo.toml              # workspace root
├── CLAUDE.md               # design notes (handoff to future contributors)
├── crates/
│   ├── proto/              # serde wire types + length-prefixed framing
│   ├── daemon/             # `screentimed` binary
│   └── tray/               # `konstantin-status` and `konstantin-tray`
├── docs/                   # user-facing reference
└── packaging/
    ├── com.gitopolis.screentimed.plist          # LaunchDaemon
    ├── com.gitopolis.konstantin-tray.plist      # per-user LaunchAgent
    ├── config.example.toml
    ├── install.sh
    ├── uninstall.sh
    ├── create-test-users.sh                     # makes alice + bob
    └── delete-test-users.sh
```

## Build

```sh
cargo build --release
cargo test  --workspace
```

To produce a `Konstantin.app` bundle (the user-facing distribution
artifact, ad-hoc codesigned for Apple Silicon):

```sh
cargo build --release
./packaging/build-app.sh                     # writes target/Konstantin.app/
open target/Konstantin.app                   # launch via Launch Services
```

The bundle is currently unsigned for distribution (no Developer ID).
For Homebrew cask distribution that's expected — cask handles the
quarantine attribute on install.

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
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/konstantin-status
```

For the operator's account (not in `[users.*]`) you'll get
`state: NotConfigured`. If you add yourself to the config with a sane
`daily_limit_minutes`, you'll see `state: Active` and a counter that
advances every `tick_seconds`.

Live-stream the same view:

```sh
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/konstantin-status --watch
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
| 6     | ✅ done     | menu-bar UI binary (`konstantin-tray`) using `objc2` + `NSStatusItem`, with auto-reconnect |
| 7     | ✅ done     | threshold notifications (e.g. 15 / 5 / 1 minutes remaining) — config-driven, fires via `osascript` |
| 8     | ❌ dropped  | lock/idle reporting — futile in user mode, see CLAUDE.md decision #3 |
| A1–A7 | ✅ done     | `Konstantin.app` bundle, admin-gated Start/Stop/Restart/Configure/Open Log/Uninstall menu, first-launch installer, Homebrew cask formula |
