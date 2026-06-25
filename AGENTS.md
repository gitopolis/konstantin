# Project context for Codex

A macOS screen-time enforcer in Rust. This file is the design handoff —
read it before making changes so decisions stay consistent.

Owner: Nikita Kutselev. Target machine: macOS Tahoe on Apple
Silicon. Rust edition 2021, MSRV 1.78. Distribution intent: Homebrew
cask shipping an unsigned `Konstantin.app` bundle. Developer ID
notarization is out of scope for v1.

The user-facing application is **Konstantin**; the privileged daemon
binary keeps its historical `screentimed` name (so do all the
`/etc/screentimed/`, `/var/db/screentimed/`, `/var/run/screentimed.sock`,
`/var/log/screentimed.log` paths and the `SCREENTIMED_*` env vars).
Bundle / cask / binary identifiers use the `com.gitopolis.*` prefix and
the `konstantin` name root.

## Architecture

Three Rust artifacts in one Cargo workspace, all eventually packaged
into a single `Konstantin.app` bundle (see "App bundle architecture"
below).

* **`screentimed`** — privileged LaunchDaemon, runs as root. Owns
  config loading, per-user counters, console-user detection via
  `SCDynamicStoreCopyConsoleUser` (SystemConfiguration framework),
  midnight resets, the `Subscribe` push channel, and forced logouts
  via the `launchctl bootout` → `pkill` escalation in
  `enforcement::force_logout`.
* **`konstantin-tray`** — per-user LaunchAgent (Aqua only). `NSStatusBar`
  / `NSStatusItem` menu-bar app subscribed to the daemon's push channel.
  Shows remaining time live, fires threshold notifications, and (in
  progress) lets the operator start / stop / restart / configure the
  daemon via menu items gated by a system admin-password prompt.
* **`konstantin-proto`** — shared library: wire types (`Request`,
  `Response`, `UserStatus`, `SessionState`) and length-prefixed JSON
  framing helpers.

A headless `konstantin-status` CLI also lives in the tray crate. Useful
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

## App bundle architecture

The end-user product is one `.app` bundle, distributed via Homebrew
cask. Bundle layout:

```
Konstantin.app/Contents/
  Info.plist                                — LSUIElement=true (no Dock icon)
  MacOS/konstantin-tray                     — main bundle executable
  Library/LaunchDaemons/com.gitopolis.screentimed.plist
  Library/LaunchAgents/com.gitopolis.konstantin-tray.plist
  Resources/
    screentimed                             — daemon binary run in place
                                              by SMAppService
    konstantin-status                       — diagnostic CLI
    config.example.toml
    AppIcon.icns
```

First-launch flow:

1. Tray attempts `UnixStream::connect("/var/run/screentimed.sock")`.
   If it succeeds, the daemon is already running — proceed with normal
   subscribe.
2. If the connect fails and the managed daemon is not enabled, the tray
   puts up a "Set up Konstantin" alert and registers the bundled
   LaunchDaemon with `SMAppService.daemon(plistName:)`.
   Dev-tree runs tell the developer to package `target/Konstantin.app`
   first instead of self-installing a development daemon.
3. The tray registers its bundled per-user LaunchAgent with
   `SMAppService.agent(plistName:)` in the current login session.

Privileged menu actions go through the daemon's admin XPC control
channel. The tray is only a signed UI client; the root daemon owns
config writes, enforcement pause, and reload.

Daemon-running detection: try to connect to the socket. Cheap, no
privileges, no `launchctl print` parsing. The worker thread's existing
`disconnected` flag drives both the menu-item enabled state and the
muted-glyph status indicator.

## Decisions locked in

These were Nikita's choices; revisit explicitly before changing them.

1. **Lockout method: soft re-logout, with escalation.** When a user
   hits zero the daemon runs `enforcement::force_logout`:
   `launchctl bootout gui/<uid>` → `launchctl bootout user/<uid>` →
   ~1 s settle → re-check `console_users()` → `pkill -KILL -U <uid>`
   if the session is still up. Plain `bootout` alone is *not*
   sufficient on macOS Tahoe — production logs have shown it return
   exit 0 for 14+ hours straight while the loginwindow session and
   `SCDynamicStoreCopyConsoleUser` keep reporting the user, so the
   escalation is mandatory. 10-second per-uid recently-kicked backoff
   prevents tight loops. No `pwpolicy` or auth-plugin tampering.

2. **Time accounting v1: foreground console user only.** Daemon
   asks `SCDynamicStoreCopyConsoleUser` who's currently at the
   console each tick and bumps that user's counter. The API returns
   0 or 1 username — Fast User Switching pauses the
   previously-foreground user automatically (the wrong-by-default
   behavior in the original `utmpx` walk, which counted everyone
   logged in). No idle / lock detection in v1. SSH / tty are not
   "console"; the API never reports them.

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
   tray," not "user sends fake reports." Foreground-console time is
   what we have; it's adversarially robust by accident
   (`SCDynamicStoreCopyConsoleUser` is daemon-owned and not
   user-controlled) and "be at the keyboard for 2 hours" is arguably
   the limit you actually want. The wire type stays in `proto` as
   dead surface — the daemon `Ack`s and ignores it. Remove it on the
   next breaking proto bump if it bothers you.

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

6. **Notifications via `UNUserNotificationCenter`.** Now that the
   bundle is Developer ID signed and notarized, threshold warnings
   attribute to "Konstantin" with the bundle icon. `notifications::
   request_authorization` runs once at tray startup so the TCC
   consent sheet appears proactively. The decision logic
   (`NotifTracker`) is unchanged from phase 7 — only the delivery
   side moved to `UNUserNotificationCenter`.
   Bare `cargo run` (no NSBundle context) still no-ops with a
   tracing warning instead of crashing.

7. **`.app` bundle is the canonical install path.** The bundle ships
   the daemon binary at `Contents/Resources/screentimed` and registers
   the bundled LaunchDaemon and tray LaunchAgent through `SMAppService`.
   No application path creates or recognizes hand-placed launchd files
   or copied executables.

8. **Privilege model: signed admin XPC.** Day-to-day privileged actions
   are authorized by the root daemon over the admin XPC Mach service:
   the peer must be the signed Konstantin tray and the peer UID must be
   an allowed operator. No persistent privilege handle, no `SMJobBless`
   helper, and no tray-owned root shell scripts.

9. **Administrative menu actions are system-wide** (like Docker
   Desktop). Pause / Unpause Enforcement, Reload Configuration, and
   Configure affect the one shared daemon/config. There is no per-user
   policy mode. Uninstall is owned by Homebrew.

10. **Status item: `clock` SF Symbol always.** When connected the
    image is a template (menu bar tints it for light/dark); when
    disconnected we apply an `NSImageSymbolConfiguration` with
    `secondaryLabelColor` and clear the template flag so the muted
    gray sticks — `NSStatusBarButton` ignores `contentTintColor` for
    template images, so the color has to be baked into the symbol.
    Connected state shows the glyph followed by formatted remaining
    time; disconnected shows the gray glyph alone. Detection is the
    worker's `disconnected` flag, driven by socket-connect success.

11. **Homebrew is the only package/update authority.** The app does
    not download releases, replace its own bundle, or expose a
    privileged update/rollback request. Homebrew owns checksums,
    application replacement, version selection, and recovery. The tray
    only reconciles ServiceManagement registration after the installed
    bundle changes.

## What's already built (phases 1–7 + A1–A9)

* **Phase 1** — proto + framing + peer-creds auth, `GetStatus` returns
  real status from current state.
* **Phase 2** — Console-user detection (originally a `utmpx` walk;
  replaced post-A7 with `SCDynamicStoreCopyConsoleUser` for FUS
  correctness and prompt logout detection — see `sessions.rs` module
  docs for the rationale), per-user counters, atomic `state.json`
  (write `<path>.tmp` + `rename(2)`), in-memory `active_now` set.
* **Phase 3** — DST-correct midnight reset task with opportunistic
  per-tick check as defense in depth.
* **Phase 4** — `Subscribe` push channel via
  `tokio::sync::broadcast::channel<()>`. Immediate snapshot on connect,
  `Lagged(n)` recovers with a fresh snapshot, no disconnect.
* **Phase 5** — enforcement: pure `decide()` function;
  `Enforcer::act_on` with kill-switch (`kill_switch_path`, default
  `/etc/screentimed/disable`) and 10 s per-uid backoff. The
  shipped `config.example.toml` sets `enforcement = "logout"`;
  the compile-time fallback when the field is missing is `"log"`.
  `force_logout` runs `bootout gui/<uid>` → `bootout user/<uid>` →
  re-check → `pkill -KILL -U <uid>` (see decision #1) with a 5 s
  per-subprocess timeout. The escalation path is unit-tested against
  a fake `LogoutRunner`; the live `launchctl` invocation has been
  verified manually against `alice` / `bob`.
* **Phase 6** — `konstantin-tray` binary using `objc2 = "0.6"`,
  `objc2-app-kit = "0.3"`, `objc2-foundation = "0.3"`, `block2 = "0.6"`.
  Main thread runs `NSApplication` and a 5 Hz `NSTimer` block; worker
  thread hosts a current-thread tokio runtime running the
  `Subscription` loop with auto-reconnect (2 s backoff).
* **Phase 7** — threshold notification decision logic. Pure
  `NotifTracker` decision logic; tracker resets on `resets_at` change,
  fires the smallest applicable threshold only, suppresses
  `LimitReached` (user is being kicked, not warned). Delivery is now via
  `UNUserNotificationCenter`.
* **A1** — `packaging/build-app.sh` produces `target/Konstantin.app/`
  from release binaries. `LSUIElement=true`, ad-hoc codesigned.
* **A2** — first-launch setup: socket-probe → optional NSAlert →
  `SMAppService.daemon(plistName:)` registration from the packaged app.
  Dev-tree runs ask the developer to package `target/Konstantin.app`.
* **A3** — reusable progress-panel primitive plus `alerts::confirm` /
  `alerts::message` siblings.
* **A4** — `actions::Controller` (NSObject subclass via
  `define_class!`) routes menu selectors such as pause/unpause,
  reload, configure, open log, and uninstall guidance.
* **A5** — state-driven UI: muted `clock` SF Symbol when
  `disconnected` (gray baked into the image via an
  `NSImageSymbolConfiguration` with `secondaryLabelColor`), the same
  glyph as a template image plus formatted time when connected. Menu
  items enable/disable from the same flag. `Latest::default()` starts
  in `disconnected: true` so initial UI is honest.
* **A6** — `bundle::Paths::resolve()` handles both real `.app` bundles
  and dev-tree (`target/<profile>/` + `packaging/`). Source labelled
  in startup logs and used to reject managed registration from a dev
  tree.
* **A7** — Homebrew owns uninstall. Its cask invokes the tray's hidden
  deferred lifecycle mode while the bundle exists. The child retains
  SMAppService handles, distinguishes upgrade/reinstall from removal by
  observing the app bundle inode, and unregisters only on real removal.
  `--zap` additionally deletes config, counters, logs, and the socket.
* **A8** — security/UX hardening on top of A1–A7:
  * `/etc/screentimed/config.toml` is now mode 0600 root-owned at
    every write site. Other users on the machine can't see whose limits
    are configured.
  * `Configure…` uses admin XPC for root-owned config reads/writes.
    Tray-agent registration is user-scoped ServiceManagement state and
    is not administered across accounts.
  * Post-password window-show uses
    `activateIgnoringOtherApps(true)` + `orderFrontRegardless()` so
    the Configure window actually steals focus from the previously
    frontmost app. `NSApplication::activate()` is cooperative on
    macOS 14+ and isn't sufficient for accessory apps.
Tests, unsafe-block count: both will drift. The CI signal is
`cargo test --workspace` clean. Don't pin counts here — they
incentivize the wrong thing.

## What's NOT built yet

The roadmap is **complete**. Phase 8 (lock/idle) was dropped — see
decision #3.

Open items that aren't on the roadmap but are worth doing before any
wider distribution:
* **Developer ID + notarization** so the `.app` bundle can ship signed,
  and so we can move to `SMAppService` and `UNUserNotificationCenter`.
* **Real `AppIcon.iconset/`** in `packaging/`. Currently the bundle
  ships with macOS's generic application icon.

The Homebrew tap is live at `github.com/gitopolis/homebrew-tap`
(cask at `Casks/konstantin.rb`); the release workflow zips, uploads,
and bumps the cask on each tag.

## Hard safety rules

These exist because the daemon can lock the operator out of their own
machine.

* **Never test on existing user accounts.** Always use the `alice` /
  `bob` test accounts created by `packaging/create-test-users.sh`.
  UIDs 601 / 602; passwords `screentime-test-alice` /
  `screentime-test-bob`.
* **The shipped `config.example.toml` now defaults to
  `enforcement = "logout"`.** When developing on your own machine,
  override that to `"log"` in your dev config (or use a separate
  `/tmp/screentimed-smoketest.toml` per the smoke-test recipe in
  `docs/concepts.md`) before running the daemon — otherwise a bug
  in your branch can actually log out an `alice` / `bob` test
  session. The default flipped because `"log"` mode shipped to end
  users is functionally a no-op product.
* **Keep `default_policy = "unrestricted"`.** Switching to `"block"`
  before every account is enumerated will kick everyone, including
  admins.
* **Never put the operator's account in `[users.*]`** unless you mean
  it. The example config only lists alice / bob for this reason.
* **Kill-switch is `/etc/screentimed/disable`** by default. Touch the
  file to suppress all enforcement actions live; remove it to
  re-enable. Configurable via `kill_switch_path` in TOML.

## Build, smoke-test, install (development workflow)

End users install via Homebrew cask. For development, build/test the
workspace and package the `.app`; the bundle's first-launch setup is
the install path.

```sh
cargo check --workspace
cargo test  --workspace
cargo build --release
./packaging/build-app.sh
open target/Konstantin.app
```

Smoke-test without installing (no root). Use an ad-hoc config (e.g.
at `/tmp/screentimed-smoketest.toml`, gitignored) — see
`docs/concepts.md` for the full recipe.

Uninstall the packaged application:

```sh
brew uninstall --cask konstantin
brew uninstall --cask --zap konstantin     # also remove all product data
```

## Layout

```
konstantin/
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
│                   src/bin/{status,tray}.rs            # konstantin-status + tray
└── packaging/
    ├── com.gitopolis.screentimed.plist          # LaunchDaemon, runs as root
    ├── com.gitopolis.konstantin-tray.plist      # per-user managed agent
    ├── config.example.toml
    ├── build-app.sh                             # app bundle builder
    └── create-test-users.sh, delete-test-users.sh
```

## macOS Tahoe gotchas

* **Code signing.** Developer ID signing/notarization is required for
  the packaged app's SMAppService registration, Notification Center
  attribution, and admin XPC peer requirements. Local ad-hoc builds are
  useful for development but are not the production install path.
* **Modern install path.** Real distribution uses
  `SMAppService.daemon(plistName:)` and `SMAppService.agent(plistName:)`
  with bundled plists. The source tree has no hand-placed launchd path.
* **Fast user switching.** `SCDynamicStoreCopyConsoleUser` reports
  only the foreground user, so backgrounded FUS users have their
  counters paused while another account is at the keyboard. State
  file still keys by username so re-foregrounding resumes from the
  same accumulated total.
* **`launchctl bootout` is best-effort and not always sufficient.**
  See decision #1 + the `enforcement::force_logout` module docs:
  bootout can return success while the loginwindow session persists,
  hence the gui+user bootout → re-check → `pkill -KILL -U <uid>`
  escalation. All steps still log failures and retry next tick.

## Style notes

* PR titles must use Conventional Commits format, for example
  `fix(updater): repair stale daemon registration`.
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
