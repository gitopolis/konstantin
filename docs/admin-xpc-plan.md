# Admin XPC control plane

## Status

The signed admin XPC control plane is the only application-owned path for
privileged day-to-day operations. Package installation, updates, downgrade,
and removal belong to Homebrew. Service registration belongs to
`SMAppService`.

## Channels

Konstantin deliberately has two IPC channels:

- `/var/run/screentimed.sock` is world-connectable and authenticates with
  `getpeereid(2)`. It returns only caller-scoped status and subscription data.
- `com.gitopolis.screentimed.control` is a root daemon Mach service for
  administrative requests. The daemon checks both the peer UID and the signed
  Konstantin tray identity.

The tray never sends a claimed UID for authorization and never receives a
persistent privilege token.

## Admin protocol

The shared JSON protocol currently supports:

- `GetConfig`
- `ValidateConfig { toml }`
- `SetConfig { toml }`
- `ReloadDaemon`
- `GetDaemonInfo`
- `GetEnforcementState`
- `SetEnforcementPaused { paused }`
- `PrepareUninstall { preserve_config }`

Configuration is root-owned and mode `0600`. Configure does not manage login
startup for other accounts: the bundled tray agent is user-scoped
ServiceManagement state and each login session registers its own agent.

`PrepareUninstall` is accepted only by the daemon using the production config
path. It removes the socket and counter state and optionally the config. It
does not stop launchd or delete an app bundle; the caller must complete
managed-service unregistration.

## ServiceManagement lifecycle

The app bundle contains:

```text
Contents/Library/LaunchDaemons/com.gitopolis.screentimed.plist
Contents/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist
```

The tray registers them with `SMAppService.daemon(plistName:)` and
`SMAppService.agent(plistName:)`. Both plists use `BundleProgram`, so the
executables run only from the app bundle.

Unregister is asynchronous. Refresh and removal wait for
`unregisterWithCompletionHandler:` before registering again or allowing the
bundle to disappear. The tray also holds a per-user file lock so registration
cannot leave two menu-bar instances running.

## Homebrew updates

Homebrew is the only bundle writer. The app contains no release downloader,
archive extractor, installer helper, or application-level rollback path.

After Homebrew replaces the app, the reopened tray checks managed status and
health. If daemon registration metadata must be refreshed, it waits for async
unregister completion, registers the bundled service, and verifies the public
socket and admin Mach service.

Recovery uses a Homebrew reinstall or versioned downgrade.

## Homebrew uninstall

Homebrew executes cask uninstall hooks during upgrades too, so the hook cannot
immediately unregister services or purge data. The cask therefore invokes:

```text
konstantin-tray --schedule-uninstall-services
```

That command starts a detached child while the app still exists. The child:

1. Resolves the current app bundle and retains both SMAppService handles.
2. Records the original bundle inode and tells the Homebrew hook it is ready.
3. Waits for Homebrew to replace or remove the bundle.
4. If a different bundle appears, treats the operation as upgrade/reinstall
   and keeps registrations intact.
5. If the bundle remains absent past the grace period, asks the daemon to
   prepare root-owned state cleanup, then asynchronously unregisters the tray
   agent and daemon.

Normal uninstall preserves configuration for reinstall. `--zap` additionally
removes configuration, counters, logs, preferences, and socket residue through
the cask.

## Verification

- Reject protocol-version mismatches and unauthorized peers.
- Round-trip every admin request/response envelope.
- Test config validation, atomic mode-0600 writes, pause state, and reload.
- Test original/replaced/missing app-bundle observations.
- Build and sign a bundle containing both managed plists.
- On a signed test install, exercise fresh registration, repair, Homebrew
  upgrade, normal uninstall, zap, and reinstall.
