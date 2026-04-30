#!/usr/bin/env bash
# Install screentimed + Konstantin tray client on the local machine.
#
# This script ASSUMES you have built the workspace already:
#     cargo build --release
#
# Usage:
#     sudo ./packaging/install.sh                     # tray for $SUDO_USER
#     sudo ./packaging/install.sh alice bob           # tray for alice + bob
#     sudo ./packaging/install.sh nikita alice bob    # tray for all three
#
# When usernames are passed, the per-user LaunchAgent is dropped into
# each user's ~/Library/LaunchAgents/ with the right ownership. Users
# who aren't currently logged in get the tray on their next login
# (`RunAtLoad=true`).
#
# Test users are created separately by `packaging/create-test-users.sh`.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${ROOT}/target/release"

# Drop a per-user tray LaunchAgent for $1, owned by them, and bootstrap
# it now if their Aqua session is active.
install_tray_for_user() {
    local user="$1"
    local uid home agents_dir dst

    if ! id "$user" >/dev/null 2>&1; then
        echo "  skipping '${user}': no such user" >&2
        return 0
    fi
    uid=$(id -u "$user")
    home=$(dscl . -read "/Users/${user}" NFSHomeDirectory 2>/dev/null \
        | awk '{print $2}' || true)
    if [[ -z "$home" || ! -d "$home" ]]; then
        echo "  skipping '${user}': no home directory" >&2
        return 0
    fi

    agents_dir="${home}/Library/LaunchAgents"
    dst="${agents_dir}/com.gitopolis.konstantin-tray.plist"

    # Create LaunchAgents/ owned by the user, then drop the plist with
    # matching ownership so the user can manage it without sudo.
    install -d -o "$user" -g staff -m 0755 "$agents_dir"
    install -m 0644 -o "$user" -g staff \
        "${ROOT}/packaging/com.gitopolis.konstantin-tray.plist" "$dst"
    echo "  installed for ${user} (uid ${uid}) → ${dst}"

    # `gui/<uid>` only exists while the user has an active Aqua session.
    # If they're logged in right now (typically the operator running
    # sudo), boot the agent up immediately. Otherwise launchd picks it
    # up at next login via `RunAtLoad`.
    if launchctl print "gui/${uid}" >/dev/null 2>&1; then
        launchctl bootout "gui/${uid}/com.gitopolis.konstantin-tray" 2>/dev/null || true
        if launchctl bootstrap "gui/${uid}" "$dst" 2>/dev/null; then
            launchctl enable "gui/${uid}/com.gitopolis.konstantin-tray" 2>/dev/null || true
            launchctl kickstart "gui/${uid}/com.gitopolis.konstantin-tray" 2>/dev/null || true
            echo "    bootstrapped into gui/${uid}"
        else
            echo "    bootstrap failed; will retry at next login" >&2
        fi
    else
        echo "    no active GUI session — will load at next login"
    fi
}

for bin in screentimed konstantin-status konstantin-tray; do
    if [[ ! -x "${TARGET}/${bin}" ]]; then
        echo "missing ${TARGET}/${bin} — run 'cargo build --release' first" >&2
        exit 1
    fi
done

echo "→ ensuring install directories exist"
# macOS doesn't ship /usr/local/libexec, and BSD `install` won't create
# parent dirs on its own. Same goes for the LaunchDaemons directory on
# minimal systems.
install -d -m 0755 /usr/local/libexec
install -d -m 0755 /usr/local/bin
install -d -m 0755 /Library/LaunchDaemons

echo "→ installing daemon binary"
install -m 0755 "${TARGET}/screentimed"        /usr/local/libexec/screentimed
echo "→ installing CLI client"
install -m 0755 "${TARGET}/konstantin-status"  /usr/local/bin/konstantin-status
echo "→ installing menu-bar app"
install -m 0755 "${TARGET}/konstantin-tray"    /usr/local/bin/konstantin-tray

echo "→ creating /etc/screentimed/"
install -d -m 0755 /etc/screentimed
if [[ ! -e /etc/screentimed/config.toml ]]; then
    install -m 0600 "${ROOT}/packaging/config.example.toml" /etc/screentimed/config.toml
    echo "  wrote /etc/screentimed/config.toml from example"
else
    echo "  /etc/screentimed/config.toml already exists, leaving it alone"
fi

echo "→ creating /var/db/screentimed/"
install -d -m 0700 /var/db/screentimed

echo "→ installing LaunchDaemon plist"
install -m 0644 "${ROOT}/packaging/com.gitopolis.screentimed.plist" \
    /Library/LaunchDaemons/com.gitopolis.screentimed.plist

echo "→ loading LaunchDaemon"
launchctl bootstrap system /Library/LaunchDaemons/com.gitopolis.screentimed.plist || true
launchctl enable system/com.gitopolis.screentimed || true
launchctl kickstart -k system/com.gitopolis.screentimed || true

# --- per-user tray LaunchAgent --------------------------------------
echo "→ installing per-user tray LaunchAgent(s)"
TRAY_USERS=("$@")
if [[ ${#TRAY_USERS[@]} -eq 0 ]]; then
    if [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
        TRAY_USERS=("${SUDO_USER}")
        echo "  no users passed; defaulting to \$SUDO_USER (${SUDO_USER})"
    else
        echo "  no users passed and \$SUDO_USER unset; skipping tray install"
        echo "  (re-run with explicit usernames, e.g.:"
        echo "       sudo ./packaging/install.sh alice bob"
        echo "   or run ./packaging/install-tray.sh as the target user)"
    fi
fi
for user in "${TRAY_USERS[@]}"; do
    install_tray_for_user "$user"
done

cat <<EOF

installed. tail the daemon log with:
    tail -f /var/log/screentimed.log

config lives at /etc/screentimed/config.toml — keep enforcement = "log"
until you've verified the daemon is making the decisions you expect.

next steps:
  * sudo ./packaging/create-test-users.sh    # makes 'alice' and 'bob'
  * to add the tray to more users later:
        sudo ./packaging/install.sh alice bob
EOF
