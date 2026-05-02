# Project context for Claude

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

## App bundle architecture (in progress)

The end-user product is one `.app` bundle, distributed via Homebrew
cask. Bundle layout:

```
Konstantin.app/Contents/
  Info.plist                                — LSUIElement=true (no Dock icon)
  MacOS/konstantin-tray                     — main bundle executable
  Library/LaunchDaemons/com.gitopolis.screentimed.plist
  Resources/
    screentimed                             — daemon binary (copied to
                                              /usr/local/libexec/ at install)
    konstantin-status                       — diagnostic CLI
    config.example.toml
    AppIcon.icns
```

First-launch flow:

1. Tray attempts `UnixStream::connect("/var/run/screentimed.sock")`.
   If it succeeds, the daemon is already running — proceed with normal
   subscribe.
2. If the connect fails AND `/Library/LaunchDaemons/com.gitopolis.screentimed.plist`
   is missing, the tray puts up a "Set up Konstantin" alert that runs
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

6. **Notifications via `osascript`** (not `UNUserNotificationCenter`).
   `osascript` is signed by Apple, so notifications work without TCC
   consent or bundle signing. Trade: notifications attribute to
   "Script Editor" rather than "Konstantin". `UNUserNotificationCenter`
   is the future path once Developer ID signing is in place.

7. **`.app` bundle is the canonical install path.** The bundle ships
   the daemon binary at `Contents/Resources/screentimed`. On
   "Set up Konstantin", the binary is **copied** (not symlinked) into
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

10. **Status item: `clock` SF Symbol always.** When connected the
    image is a template (menu bar tints it for light/dark); when
    disconnected we apply an `NSImageSymbolConfiguration` with
    `secondaryLabelColor` and clear the template flag so the muted
    gray sticks — `NSStatusBarButton` ignores `contentTintColor` for
    template images, so the color has to be baked into the symbol.
    Connected state shows the glyph followed by formatted remaining
    time; disconnected shows the gray glyph alone. Detection is the
    worker's `disconnected` flag, driven by socket-connect success.

## What's already built (phases 1–7 + A1–A7)

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
* **Phase 7** — threshold notifications via `osascript`. Pure
  `NotifTracker` decision logic; tracker resets on `resets_at` change,
  fires the smallest applicable threshold only, suppresses
  `LimitReached` (user is being kicked, not warned).
* **A1** — `packaging/build-app.sh` produces `target/Konstantin.app/`
  from release binaries. `LSUIElement=true`, ad-hoc codesigned.
* **A2** — first-launch install: socket-probe → optional NSAlert →
  privileged install via `osascript` on a background thread, with
  the main thread pumping the run loop and showing a progress panel
  so the cursor stays normal.
* **A3** — `admin::run_with_progress` primitive lifted out;
  reusable shape `Result<(), admin::Error::{Cancelled, Failed}>`.
  `alerts::confirm` / `alerts::message` siblings.
* **A4** — `actions::Controller` (NSObject subclass via
  `define_class!`) routes `startDaemon: / stopDaemon: / restartDaemon:
  / configure: / openLog: / uninstall:` selectors. Lifecycle commands
  use `|| true`-tolerant launchctl chains so already-loaded /
  already-stopped states are idempotent.
* **A5** — state-driven UI: muted `clock` SF Symbol when
  `disconnected` (gray baked into the image via an
  `NSImageSymbolConfiguration` with `secondaryLabelColor`), the same
  glyph as a template image plus formatted time when connected. Menu
  items enable/disable from the same flag. `Latest::default()` starts
  in `disconnected: true` so initial UI is honest.
* **A6** — `bundle::Paths::resolve()` handles both real `.app` bundles
  and dev-tree (`target/<profile>/` + `packaging/`). Source labelled
  in startup log. User LaunchAgent rewrite is bundle-only — running
  `target/release/konstantin-tray` directly no longer poisons
  `~/Library/LaunchAgents/` with a dev path.
* **A7** — `Uninstall…` menu item: confirm → privileged teardown
  (`launchctl bootout` + `rm` of system files) → user LaunchAgent
  cleanup → `NSApplication::terminate`. The cask formula in the
  separate tap repo (`github.com/gitopolis/homebrew-konstantin`,
  `Casks/konstantin.rb`) carries matching `uninstall` + `zap` stanzas
  for non-interactive `brew uninstall --zap`; the release workflow
  auto-bumps version + sha256 there on each tag.
* **A8** — security/UX hardening on top of A1–A7:
  * `/etc/screentimed/config.toml` is now mode 0600 root-owned at
    every write site (`packaging/install.sh`, the tray's first-launch
    `install::build_install_script`, and the Save flow's
    `config_ui::build_admin_script`). Other users on the machine
    can't see whose limits are configured.
  * `Configure…` therefore prompts for an admin password to *open*
    the window: a single `admin::run_with_progress` invocation
    (`config_ui::build_open_admin_script`) copies the config out to
    a user-owned temp *and* dumps a manifest of per-user
    LaunchAgent-plist presence (`<username> 0|1` lines) in the same
    elevation. Drops the old operator-owned `UiCache` (the
    `~/Library/Application Support/com.gitopolis.konstantin/ui-state.json`
    file) — root can stat hardened homes directly, no cache needed.
  * `Uninstall…` and `packaging/uninstall.sh` now `rm -rf
    /var/db/screentimed/` (counter state). `/etc/screentimed/`
    (config) is still preserved on uninstall; `--zap` still removes
    both. Cask `uninstall` stanza updated to match.
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

The Homebrew tap is live at `github.com/gitopolis/homebrew-konstantin`
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
    ├── com.gitopolis.konstantin-tray.plist      # LaunchAgent, Aqua-only
    ├── config.example.toml
    ├── install.sh, uninstall.sh                 # system-side, requires sudo
    ├── install-tray.sh, uninstall-tray.sh       # per-user, no sudo
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
