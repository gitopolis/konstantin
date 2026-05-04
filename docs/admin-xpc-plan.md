# Admin XPC Control Plane Plan

## Goal

Reduce repeated administrator password prompts for Konstantin's operator
actions while keeping `screentimed` privileged and preserving the rule
that standard users cannot stop, restart, reconfigure, uninstall, or
upgrade the daemon.

The target end state is:

* First setup/registration may require one Apple-managed authorization.
* Day-to-day operator actions go tray -> root daemon over a signed XPC
  control channel, without `osascript`.
* The daemon authorizes each mutating request using the connecting
  process identity and the real operator account.
* Existing status subscription behavior stays available to all users.

Developer ID signing and notarization are already stable in the release
workflow, so the plan assumes signed-release builds can use XPC peer
code-signing requirements.

## Current State

Privileged tray actions currently build shell snippets and pass them to
`admin::run_with_progress`, which runs:

```text
osascript -e 'do shell script "..." with administrator privileges'
```

This affects at least:

* Restart / Reload Daemon.
* Open Configuration, because `/etc/screentimed/config.toml` is
  `0600 root`.
* Save Configuration, because it writes `/etc/screentimed/config.toml`,
  edits other users' LaunchAgents, and restarts the daemon.
* Pause / Unpause Enforcement, once the tray exposes it as a direct menu
  action.
* First setup, uninstall, and updater install.

The daemon already authenticates status clients with `getpeereid(2)` on
the Unix socket at `/var/run/screentimed.sock`, but that socket is mode
`0666` by design and currently only serves caller-scoped status requests.

## Platform Facts To Design Around

Apple's modern ServiceManagement guidance for macOS 13+ is to bundle
LaunchDaemons in:

```text
Konstantin.app/Contents/Library/LaunchDaemons/
```

and use `SMAppService.daemon(plistName:)` to register them. Apple's
migration guidance also says bundled launchd plists should use
`BundleProgram` instead of an absolute `Program`, with the executable
path relative to the app bundle.

LaunchDaemons still run as root and still require user authorization
because the user is approving a system-level process. That makes
`SMAppService` a good replacement for first-launch setup, not a general
replacement for every later privileged operation.

XPC is the right control channel for the signed app case. Apple exposes
peer requirements such as:

* `xpc_connection_set_peer_team_identity_requirement`, which requires
  the peer executable to be signed by the same Team ID and optionally a
  specific signing identifier.
* Newer peer requirement APIs such as `xpc_listener_set_peer_requirement`
  / `xpc_session_set_peer_requirement`.

The key security point: code-signing checks prove "this process is the
signed Konstantin tray", while peer credentials prove "this connection
came from operator UID N". We need both.

References:

* https://developer.apple.com/documentation/servicemanagement/smappservice
* https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos
* https://developer.apple.com/documentation/xpc/xpc_connection_set_peer_team_identity_requirement%28_%3A_%3A%29?language=objc
* https://developer.apple.com/documentation/xpc/xpc_listener_set_peer_requirement
* https://developer.apple.com/documentation/security/authorization-services

## Proposed Architecture

Keep two channels:

1. Public status channel, unchanged for v1.
   `/var/run/screentimed.sock`, mode `0666`, length-prefixed JSON, any
   user can connect, daemon scopes all status to the peer UID.

2. Admin control channel, new.
   A launchd-registered XPC Mach service owned by `screentimed`, exposed
   only by the root daemon. Mutating operations are accepted only when:
   * the client process satisfies Konstantin's code-signing requirement;
   * the peer EUID resolves to a real local user;
   * that user is currently an operator, initially defined as membership
     in the local `admin` group;
   * the requested operation passes operation-specific validation.

The daemon remains the only process that touches root-owned state. The
tray becomes a UI client for admin actions instead of a shell launcher.

## Control API Shape

Introduce a control protocol separate from `konstantin-proto`'s public
status protocol. This avoids making every standard user deserialize
admin-only wire types and lets the control channel evolve independently.

Suggested Rust module:

```text
crates/daemon/src/control/
  mod.rs
  auth.rs
  config.rs
  launchd.rs
  xpc.rs
```

Suggested request/response model:

```rust
enum AdminRequest {
    GetConfig { autostart_probes: Vec<TrayAutostartProbe> },
    ValidateConfig { toml: String },
    SetConfig {
        toml: String,
        tray_exe: PathBuf,
        tray_autostart: Vec<TrayAutostartChange>,
    },
    ReloadDaemon,
    GetEnforcementState,
    SetEnforcementPaused { paused: bool },
    RestartDaemon,
    GetInstallState,
    Uninstall { preserve_config: bool },
}

enum AdminResponse {
    Config {
        toml: String,
        enforcement_paused: bool,
        kill_switch_path: PathBuf,
        tray_autostart: Vec<TrayAutostartState>,
    },
    EnforcementState { paused: bool, kill_switch_path: PathBuf },
    Ok,
    InstallState(InstallState),
    ValidationErrors(Vec<String>),
    Unauthorized { reason: String },
    Error { message: String },
}
```

Do not add normal Start/Stop daemon controls. Product semantics should be
that `screentimed` always runs after setup and until uninstall. The tray
can still show disconnected/error state if the daemon is missing or
crashed, but routine operator control should be "Pause Enforcement" /
"Unpause Enforcement", not unloading the LaunchDaemon.

Restart should be rare. Prefer `ReloadDaemon` after configuration saves.
Keep `RestartDaemon` only if there is still a developer/operator command
that genuinely needs a process restart.

## Enforcement Pause Semantics

The daemon already supports a kill switch: if `kill_switch_path` exists
in the loaded config, enforcement actions are suppressed. The default is:

```text
/etc/screentimed/disable
```

Use this existing mechanism for the tray's Pause/Unpause Enforcement UI.
The root daemon should create or delete the configured kill-switch file
on behalf of an authorized operator.

New flow:

1. Tray asks `GetEnforcementState`.
2. Daemon replies with whether the configured kill-switch file exists.
3. Menu item shows `Pause Enforcement` when absent and
   `Unpause Enforcement` when present.
4. Tray sends `SetEnforcementPaused { paused: true }`.
5. Daemon creates parent directories as needed and writes/touches the
   kill-switch file with root ownership and restrictive mode.
6. Tray sends `SetEnforcementPaused { paused: false }`.
7. Daemon removes only the exact configured kill-switch path.
8. Each connected tray periodically refreshes `GetEnforcementState` while
   the daemon is reachable, so other admin trays converge on the new menu
   state without a password prompt.

This avoids fighting `KeepAlive`, keeps the privileged control plane
available, and reuses the existing safety mechanism operators already
know from the development workflow.

Validation rules:

* The kill-switch path comes only from daemon config, never from the tray
  request.
* If `kill_switch_path` is relative, empty, or otherwise invalid, reject
  the operation and log a configuration error.
* Removing the kill switch must tolerate `ENOENT` and still return
  success.
* Creating the kill switch should be atomic enough for the local file
  case: create/write a temp sibling, set mode/owner, then rename.

## Authorization Policy

Phase 1 policy:

* The XPC peer executable must be signed by the same Developer Team and
  have the Konstantin tray signing identifier, e.g.
  `com.gitopolis.konstantin`.
* The peer EUID must resolve to a local account.
* The account must be in `/Groups/admin`.
* Root is allowed for diagnostic CLI/testing.

Phase 2 optional policy:

* Add `/etc/screentimed/operators.toml` with an explicit allowlist:

```toml
admins_allowed = true
users = ["nikita"]
groups = ["konstantin-operators"]
```

Default remains `admins_allowed = true` to match the user's requirement.
Do not parse sudoers. Sudo policy can include host, command, timestamp,
LDAP, and plugin behavior; local admin-group membership is a clearer
product rule.

Authorization checks must live in the daemon, not in the tray. Tray UI
can hide or disable controls for non-admin users, but that is only UX.

## XPC Implementation Strategy

Rust does not currently have first-class, widely used safe bindings for
all modern XPC peer requirement APIs. Keep the unsafe boundary small.

Recommended implementation:

1. Add a small internal `xpc_control` module in the daemon and tray with
   `extern "C"` bindings to libxpc.
2. Use JSON payloads inside XPC dictionaries initially:
   * request dictionary keys: `version`, `request_id`, `payload_json`;
   * response dictionary keys: `request_id`, `ok`, `payload_json`,
     `error`.
3. Keep all control payload structs in Rust with serde.
4. Build one synchronous request/reply helper for the tray worker thread,
   then wrap it in the existing progress panel primitive.
5. Prefer `xpc_listener_set_peer_requirement` if available in the target
   SDK. If SDK availability or Rust bindings make that awkward, set the
   requirement per connection with
   `xpc_connection_set_peer_team_identity_requirement(connection,
   "com.gitopolis.konstantin")` before accepting messages.
6. Also read peer EUID via XPC and feed it into the daemon authorization
   function. Do not accept a UID in the request body.

Keep the public Unix-socket proto unchanged until admin XPC is working.
Afterwards, consider moving status subscription to XPC too, but that is
not required for reducing password prompts.

## ServiceManagement Migration

The app bundle is already close to the required shape:

```text
Konstantin.app/Contents/Library/LaunchDaemons/com.gitopolis.screentimed.plist
Konstantin.app/Contents/Resources/screentimed
```

Change the bundled daemon plist for SMAppService:

```xml
<key>BundleProgram</key>
<string>Contents/Resources/screentimed</string>
```

Remove the runtime copy to `/usr/local/libexec/screentimed` for the
SMAppService path. The daemon should run from inside the bundle. Keep the
legacy copy/install path as a migration fallback for existing installs
until one release after the SMAppService migration ships.

First-launch setup flow becomes:

1. Resolve bundle paths.
2. Create the user LaunchAgent through `SMAppService.agent(...)` if we
   move the tray autostart plist into `Contents/Library/LaunchAgents`,
   or keep the current per-user LaunchAgent writing for self-autostart.
3. Register the daemon with `SMAppService.daemon(plistName:
   "com.gitopolis.screentimed.plist")`.
4. If status is not enabled/authorized, call
   `SMAppService.openSystemSettingsLoginItems()` and show instructions.
5. Once the daemon is running, perform any first-run config seeding
   through admin XPC or let the daemon seed defaults as root on startup.

Important packaging implication: if the daemon runs from inside the app
bundle, updater rollback logic must verify the newly installed bundle's
daemon via SMAppService/launchd rather than copying a daemon to
`/usr/local/libexec`.

## Configuration Flow

Current configure-open needs a password only because the config is root
owned and the tray also needs cross-user LaunchAgent state.

New flow:

1. Tray opens Configure.
2. Tray enumerates local users for the UI and sends `GetConfig` over
   admin XPC with `{ username, home }` autostart probes.
3. Daemon checks authorization.
4. Daemon reads `/etc/screentimed/config.toml`.
5. Daemon stats the requested LaunchAgent paths as root, which avoids
   hardened-home permission problems.
6. Tray renders the window from the response.
7. On Save, tray sends full edited TOML, the running tray executable
   path, and autostart diffs.
8. Daemon validates:
   * TOML parses as `Config`;
   * tray executable path exists and is absolute;
   * autostart homes are absolute and do not target root;
   * limits are sane;
   * LaunchAgent paths are under the target user's home.
9. Daemon writes config atomically with `0600 root`.
10. Daemon applies LaunchAgent changes.
11. Daemon schedules `launchctl kickstart -k` after replying so the
    running XPC request can finish before launchd restarts it.
12. Existing tray subscriptions reconnect and receive fresh status from
    the restarted daemon.

This removes both Open Configuration and Save Configuration password
prompts.

## Restart / Reload Flow

Prefer in-process reload over launchd restart for most config changes.

Add:

```rust
AdminRequest::ReloadDaemon
```

Implemented shape:

* reload `/etc/screentimed/config.toml`;
* reply to the tray;
* schedule a delayed `launchctl kickstart -k` from inside the root daemon.

Reserve full process restart for upgrades or unrecoverable internal
state. The tray menu now says "Reload Configuration" so the label matches
the operator intent even though the current daemon architecture still uses
a launchd kickstart to apply the new immutable config clones.

If a true process restart is still needed, use `execve` of the current
daemon binary or ask launchd to kickstart from inside the root daemon.
This needs careful reply ordering: respond to the tray first, then spawn
a detached restart task.

## Update Flow

Keep the unprivileged parts unchanged:

* check GitHub release;
* download zip;
* verify GitHub API `digest` SHA-256;
* unpack to a per-pid temp dir.

Replace the privileged `osascript` install script with:

```rust
AdminRequest::InstallUpdate {
    staged_bundle_path,
    expected_version,
    expected_sha256,
}
```

Daemon-side install script/logic:

* validate staged path is under a Konstantin-owned temp parent;
* validate bundle code signature/notarization before swap;
* stop/re-register LaunchDaemon through the appropriate path;
* swap bundle at `bundle::Paths::resolve()?.bundle_root`;
* verify daemon comes back within the existing 20-second window;
* rollback with the current distinct exit-code semantics mapped to
  structured `AdminResponse` errors.

This is a later phase because it is the highest-risk action. Leave
`osascript` for update install until config/reload/pause are proven.

## Uninstall Flow

Uninstall is destructive and rare, so it can migrate last.

Options:

* Keep one password prompt for Uninstall indefinitely. This is acceptable
  because the goal is reducing repeated routine prompts, not necessarily
  making destructive teardown frictionless.
* Or implement `AdminRequest::Uninstall` after the XPC auth model is
  mature. The daemon performs privileged teardown after replying and
  terminates itself at the end.

If daemon-mediated uninstall is implemented, require an extra tray
confirmation even for admin users.

## Phased Implementation

Progress tracking:

* `4405f77 feat: add daemon admin control primitives` completed the
  daemon-side authorization/control-handler foundation and added this
  plan.
* `925a574 feat: scaffold admin xpc transport` added the Mach service
  plist entry and initial XPC dictionary/envelope scaffolding.
* `1238b8c feat: add admin xpc request dispatch` added pure JSON
  request dispatch through the daemon controller.
* `4611738 feat: run daemon admin xpc listener` wired the daemon-side
  XPC listener, peer EUID lookup, signed-peer requirement setup, and
  reply path.
* `4c01d90 feat: share admin xpc protocol with tray` moved the admin
  wire model into `konstantin-proto::admin` and added the tray
  `AdminClient`.
* `baca055 feat: add enforcement pause menu action` replaced routine
  Start/Stop Daemon menu items with Pause/Unpause Enforcement over admin
  XPC.
* `9447551 feat: configure settings over admin xpc` replaced the
  Configure open/save `osascript` paths with admin XPC, including
  daemon-owned config writes and daemon-owned tray LaunchAgent autostart
  changes.

### Phase 0: Documentation and scaffolding

Status: complete.

* [x] Add this plan.
* Add an `xpc-control` Cargo feature or cfg gate if needed.
  * Not added yet; current code is always compiled on macOS and has
    non-macOS stubs where needed.
* [x] Add internal docs explaining that status IPC and admin control IPC have
  different threat models.

### Phase 1: Daemon-side authorization primitives

Status: mostly complete; audit logging can be tightened during the
Configure-over-XPC work.

* [x] Add `control::auth`.
* [x] Implement `operator_from_uid(uid) -> Operator`.
  * Implemented as an `Operator` snapshot with `allowed` + `reason`
    rather than a fallible return type, so denied users can be returned
    as structured `Unauthorized` responses.
* [x] Implement local admin-group lookup using `dscl`;
  tests should use injectable command/output helpers.
  * Parser is unit-tested. The live command wrapper is intentionally
    small; a fully injectable command runner can be added if this grows.
* [ ] Add structured audit logging for allowed and denied admin attempts.
  * Current listener logs peer EUID, username, and allowed/denied state
    at debug level.
* [x] Unit-test standard user denied behavior at the controller boundary.
* [ ] Add direct unit coverage for admin/root/unknown UID resolution.

### Phase 2: Control protocol without XPC

Status: complete for the initial scope.

* [x] Add Rust request/response structs.
  * Initially added in the daemon, then moved to
    `konstantin-proto::admin` so the tray can share them.
* [x] Add pure handlers for `GetConfig`, `ValidateConfig`, `SetConfig`, and
  `ReloadDaemon`.
* [x] Add pure handlers for `GetEnforcementState` and
  `SetEnforcementPaused`.
* [x] Exercise handlers directly in tests, without transport.
* [x] Preserve current `osascript` update/restart paths while daemon
  handlers mature.
  * Configure open/save has now moved to admin XPC in Phase 4.

### Phase 3: XPC transport

Status: implemented, but still needs signed-app manual verification.

* [x] Add minimal libxpc bindings.
* [x] Register a daemon Mach service:

```text
com.gitopolis.screentimed.control
```

* [x] Add the matching `MachServices` key to the LaunchDaemon plist.
* [x] Enforce Konstantin peer code-signing requirement.
  * Daemon requires peer Team ID plus tray signing identifier.
  * Tray requires same-Team daemon peer.
* [x] Extract peer EUID from XPC and feed daemon authorization.
* [x] Build tray `AdminClient` with request/reply.
* [ ] Add timeout handling around tray requests.
  * Current implementation uses `xpc_connection_send_message_with_reply_sync`
    from a worker thread; it can block if the daemon stalls.
* [ ] Add a signed-build manual test recipe.

### Phase 4: Configure over XPC

Status: implemented; still needs signed-app manual verification.

* [x] Replace Configure Open admin script with `GetConfig`.
  * `GetConfig` now accepts tray-autostart probes so the root daemon can
    stat other users' LaunchAgent plists without staging a manifest.
* [x] Replace Save admin script with `SetConfig`.
  * `SetConfig` now carries the tray executable path plus autostart
    changes. The daemon writes `/etc/screentimed/config.toml`, applies
    LaunchAgent changes, then schedules a delayed `launchctl kickstart`
    so the XPC reply can return before the daemon restarts.
* [x] Keep UI behavior and validation messages as close as possible to the
  current flow.
* [x] Remove staged temp config/manifest files from the tray path.
* [ ] Manual signed-app check: admin user opens Configure and saves with
  no password prompt; standard user receives `Unauthorized`.

### Phase 5: Reload / pause controls

Status: partially complete.

* [x] Replace Restart with `ReloadDaemon` where possible.
  * The tray menu now calls admin XPC `ReloadDaemon` as `Reload
    Configuration`. The daemon validates the config, replies, then
    schedules a delayed launchd kickstart when running against the system
    config path.
* [x] Remove routine Start/Stop Daemon menu items.
* [x] Add Pause Enforcement / Unpause Enforcement menu item backed by
  `GetEnforcementState` and `SetEnforcementPaused`.
* [x] Reflect the kill-switch state in the menu so every admin tray shows
  whether enforcement is currently paused.
  * Connected trays poll `GetEnforcementState` every 30 seconds; the
    acting tray still updates immediately from the XPC response.
* [ ] Add a daemon status/control field if the tray needs to distinguish
  disconnected/running/enforcement-paused.
  * Deferred for now because periodic admin-XPC refresh keeps the current
    menu accurate without changing the public status socket.

### Phase 6: ServiceManagement first-launch setup

* Convert bundled plist to `BundleProgram`.
* Add `SMAppService.daemon(plistName:)` registration path.
* Keep legacy `osascript` installer for migration/fallback only.
* Add upgrade logic that recognizes old `/Library/LaunchDaemons` and
  `/usr/local/libexec` installs and migrates them cleanly.

### Phase 7: Updates and uninstall

* Move updater install to daemon-mediated XPC once lower-risk admin
  operations are stable.
* Decide whether uninstall remains one prompted `osascript` path or moves
  to XPC.
* Remove `admin::run_with_progress` only when every retained action has a
  replacement or an explicit fallback reason.

## Testing Plan

Automated tests:

* `cargo test --workspace`.
* Auth parser tests for admin membership.
* Control handler tests with temp config/state dirs.
* Config write tests asserting mode `0600`.
* Denial tests asserting mutating operations do not touch files.
* LaunchAgent path validation tests to prevent writes outside a target
  user's home.

Manual signed-build tests:

* Install signed/notarized Konstantin.
* Standard user can see own status but cannot open Configure.
* Admin user opens Configure with no password prompt.
* Admin user saves limits with no password prompt.
* Standard user cannot forge admin requests with a local script.
* Unsigned/dev tray cannot use release daemon's admin XPC.
* Ad-hoc dev build has either an explicit dev-only bypass or a clear
  "admin XPC unavailable in unsigned builds" behavior.
* Pause/Unpause Enforcement creates and removes only the configured
  kill-switch file.
* Paused enforcement suppresses logout actions but leaves status,
  config, and admin XPC reachable.
* Update rollback still works from arbitrary bundle locations.

Safety smoke tests:

* Never use the operator account in `[users.*]`.
* Use `alice` / `bob` test accounts only.
* Keep dev configs in log mode or keep the kill switch touched.
* Verify `default_policy = "unrestricted"` remains unchanged by config
  round-tripping.

## Migration And Compatibility

Support three installation states during migration:

1. Legacy script install:
   `/Library/LaunchDaemons/com.gitopolis.screentimed.plist` points to
   `/usr/local/libexec/screentimed`.
2. New SMAppService install:
   launchd registration points to the app-bundled daemon via
   `BundleProgram`.
3. Dev-tree run:
   no signed XPC trust. Keep current shell/admin fallback or disable admin
   XPC explicitly with a useful message.

Do not strand users on the legacy install. The signed app should detect
legacy state and offer a one-time migration.

## Risks

* XPC bindings in Rust will require unsafe FFI. Keep the module small and
  heavily tested at the serialization/auth boundary.
* Peer signing requirements must be tested against the actual Developer
  ID release artifact, not only ad-hoc local builds.
* Pause/Unpause must not become a generic privileged file-write
  primitive; the daemon must use only the configured kill-switch path.
* Running the daemon from inside the app bundle changes updater and
  rollback assumptions.
* Admin-group membership is simpler than sudo policy but not identical to
  "can run sudo". This is likely the right product rule; document it.

## Recommended First PR

The first implementation PR should avoid XPC and focus on daemon-side
control semantics:

* add `control::auth`;
* add admin request/response structs;
* implement and test pure `GetConfig`, `ValidateConfig`, `SetConfig`,
  and `ReloadDaemon` handlers against temp dirs;
* keep the tray untouched except for any shared type imports.

That gives us the security-sensitive file/config behavior under test
before adding the transport layer.
