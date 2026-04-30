# Configuration

The daemon reads its config from the path in `$SCREENTIMED_CONFIG` if
set, otherwise from `/etc/screentimed/config.toml`. Format is TOML.

The installed config is **mode 0600 root-owned** — only root can read
or write it. There are two ways to edit it:

* **Menu-bar `Configure…`** (recommended). Prompts for an admin
  password, opens a native window populated from the current config,
  and on Save writes back atomically and `launchctl kickstart -k`s the
  daemon for you. The Configure window also lets you toggle each
  user's per-user tray (`Start at login`).
* **Hand edit as root**, e.g. `sudo $EDITOR /etc/screentimed/config.toml`,
  then reload with
  `sudo launchctl kickstart -k system/com.gitopolis.screentimed`.

A complete annotated example lives in
[`packaging/config.example.toml`](../packaging/config.example.toml).

## Top-level fields

### `socket_path`

* **Type**: string (filesystem path)
* **Default**: `/var/run/screentimed.sock`

Where the daemon binds its Unix socket. The daemon `chmod`s it to `0666`
on bind so any local user can connect; authentication is via peer
credentials, not file mode (see [concepts.md](concepts.md#ipc-framing--peer-creds)).
A stale socket from a previous run is removed on startup.

### `state_path`

* **Type**: string (filesystem path)
* **Default**: `/var/db/screentimed/state.json`

Where per-user counters are persisted. Written atomically via
`<path>.tmp` + `rename(2)` after each tick. The parent directory is
created if missing. Format:

```json
{
  "day": "2026-04-26",
  "counters": { "alice": 1842 }
}
```

If the file is missing or unparseable on startup, the daemon starts
with empty counters (logged as a WARN). If the `day` field is older
than today, the startup path calls `reset_if_new_day(today)` before
the IPC server accepts its first connection, so subscribers never see
yesterday's totals.

### `tick_seconds`

* **Type**: positive integer (seconds)
* **Default**: `5`
* **Validation**: must be `> 0`

How often the daemon enumerates console sessions, increments counters,
persists state, and broadcasts to subscribers. Lower values give a
livelier countdown but cost more I/O and CPU. `5` is a reasonable
production default; `2` is convenient for smoke testing.

### `warn_thresholds_minutes`

* **Type**: array of positive integers (minutes)
* **Default**: `[15, 5, 1]`

Minutes-remaining values at which the *tray* (phase 7) will pop a
notification. The daemon does not act on these directly — it only
reports `remaining_seconds`. Notifications are tray-side so the user
controls them; the daemon stays UI-free.

### `default_policy`

* **Type**: enum string — `"unrestricted"` or `"block"`
* **Default**: `"unrestricted"`

What happens to users who are logged in to a console session but not
listed in `[users.*]`:

* `"unrestricted"` — they are tracked (counter advances) but never
  kicked. Their `SessionState` is `NotConfigured`.
* `"block"` — they are kicked on every tick (subject to backoff).
  `KickReason::BlockedByDefaultPolicy`.

**Safety**: do not flip to `"block"` until you have enumerated *every*
user account that should be allowed in (including admin accounts).
Otherwise the daemon will boot you off your own machine.

### `enforcement`

* **Type**: enum string — `"log"` or `"logout"`
* **Shipped default** (in `packaging/config.example.toml`, written to
  `/etc/screentimed/config.toml` on first launch): `"logout"`
* **Compile-time fallback** if the field is missing from the config
  entirely: `"log"`

Master safety switch. Determines what happens when `decide()` returns
`Kick`:

* `"logout"` — invoke `/bin/launchctl bootout user/<uid>` (with a 5 s
  timeout). On failure, log + retry next tick. This is the shipped
  default — the whole point of installing this is to enforce.
* `"log"` — write `would have kicked X (used=N, reason=...)` to the
  log. Never spawns `launchctl`. Useful while bringing the daemon up
  on a new machine, or while you're tuning per-user limits.

Both modes honor the kill-switch and the recently-kicked backoff. Both
modes update the per-uid backoff timestamp on success.

### `kill_switch_path`

* **Type**: string (filesystem path)
* **Default**: `/etc/screentimed/disable`

Live kill-switch. If this file exists, the daemon refuses to act on a
`Kick` decision regardless of `enforcement`. Touch it to disable
enforcement immediately; remove it to re-enable. No restart needed.

The check happens *after* `decide` returns `Kick`, so log-mode
"would have kicked" entries are also suppressed when the switch is
active (replaced by "kill-switch present, refusing to enforce" WARNs).

For local smoke testing without root, point this somewhere writable:

```toml
kill_switch_path = "/Users/me/projects/screentime/run/disable"
```

## `[users.<name>]`

Every account that should have a daily limit gets a section here. The
key (`alice`, `bob`, …) is the local Unix username — what `whoami`
returns when logged in to that account. The daemon resolves it to a UID
via `getpwnam` only when it's about to enforce.

### `daily_limit_minutes`

* **Type**: positive integer (minutes)
* **Validation**: must be `> 0`
* **Required**

Daily allowance in minutes. Counter resets at local midnight. There's
no upper bound — `1440` (24 h) effectively means "tracked but never
kicked", which is useful for putting yourself in the config without
imposing a real limit.

## Complete example

```toml
socket_path  = "/var/run/screentimed.sock"
state_path   = "/var/db/screentimed/state.json"
tick_seconds = 5

warn_thresholds_minutes = [15, 5, 1]
default_policy = "unrestricted"
enforcement    = "logout"

kill_switch_path = "/etc/screentimed/disable"

[users.alice]
daily_limit_minutes = 30

[users.bob]
daily_limit_minutes = 60
```

## Override paths via environment

For development or smoke testing, set `SCREENTIMED_CONFIG` to point at
an alternate config file. The path takes precedence over
`/etc/screentimed/config.toml`. See [cli.md](cli.md#screentimed) for
the full env-var list.
