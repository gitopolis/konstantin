# screentime

A macOS screen-time enforcer written in Rust.

* `screentimed` — privileged LaunchDaemon. Reads config, tracks per-user
  used-seconds, kicks users over their daily limit via `launchctl bootout`.
* `screentime-status` — headless CLI client. Asks the daemon "what's my
  status?" over a Unix socket. Will be joined by `screentime-tray` (a
  menu-bar app) in a follow-up commit.

This is a **scaffold** as of this commit. The daemon listens on a socket
and answers `GetStatus` with stub values; session polling, logout, the
midnight reset task, and the menu-bar UI all land in subsequent phases.

## Layout

```
screentime/
├── Cargo.toml              # workspace root
├── crates/
│   ├── proto/              # serde wire types + length-prefixed framing
│   ├── daemon/             # `screentimed` binary
│   └── tray/               # `screentime-status` (and later `screentime-tray`)
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
```

## Smoke-test (no install required)

In one terminal:

```sh
mkdir -p ./run
SCREENTIMED_CONFIG=./packaging/config.example.toml \
SCREENTIMED_LOG=debug \
./target/release/screentimed
```

…but first edit `packaging/config.example.toml` and change `socket_path` to
something writable like `./run/screentimed.sock` for local testing.

In another terminal:

```sh
SCREENTIMED_SOCKET=./run/screentimed.sock ./target/release/screentime-status
```

Expected output (the user is `nikita`, who is not in the example config):

```
user      : nikita (uid 501)
state     : NotConfigured
daily     : 0s
used      : 0s
remaining : 0s
resets_at : 2026-04-27 00:00:00 -0700
```

If you put `nikita` (or `alice`, `bob`) in the config, you'll get
`state: Active` and the configured limit instead.

## Install on this machine

```sh
sudo ./packaging/create-test-users.sh   # creates alice (uid 601), bob (uid 602)
cargo build --release
sudo ./packaging/install.sh             # installs binaries + plists, starts daemon
```

The example config has `enforcement = "log"`, so even after session polling
and bootout are wired up, no one will actually be kicked until you flip that
to `"logout"`. Keep it in `"log"` mode while developing.

To remove everything:

```sh
sudo ./packaging/uninstall.sh
sudo ./packaging/delete-test-users.sh
```

## Roadmap

| Phase | Status      | What lands |
|-------|-------------|------------|
| 1     | ✅ done     | proto + framing + peer-creds-authenticated stub IPC |
| 2     | next        | utmpx session enumeration, per-user counters, JSON state file |
| 3     |             | midnight reset task |
| 4     |             | `Subscribe` push channel for live tray updates |
| 5     |             | forced logout via `launchctl bootout` (gated by `enforcement = "logout"`) |
| 6     |             | menu-bar UI binary (`screentime-tray`) using `objc2` + `NSStatusItem` |
| 7     |             | threshold notifications |
| 8     |             | (v2) per-user lock/idle reporting via the tray |
