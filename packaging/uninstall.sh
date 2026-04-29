#!/usr/bin/env bash
# Remove the system-side install of screentimed (daemon + binaries),
# plus per-user tray LaunchAgents for the listed users.
#
# Leaves /etc/screentimed and /var/db/screentimed intact so you don't
# lose configs / state on a reinstall.
#
# Usage:
#     sudo ./packaging/uninstall.sh                   # tray for $SUDO_USER
#     sudo ./packaging/uninstall.sh alice bob         # tray for alice + bob
#     sudo ./packaging/uninstall.sh nikita alice bob

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

uninstall_tray_for_user() {
    local user="$1"
    local uid home dst

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
    dst="${home}/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist"

    if launchctl print "gui/${uid}" >/dev/null 2>&1; then
        launchctl bootout "gui/${uid}/com.gitopolis.konstantin-tray" 2>/dev/null || true
    fi
    rm -f "$dst"
    echo "  removed tray for ${user}"
}

echo "→ unloading LaunchDaemon"
launchctl bootout system/com.gitopolis.screentimed 2>/dev/null || true

echo "→ removing plists"
rm -f /Library/LaunchDaemons/com.gitopolis.screentimed.plist
# Pre-phase-7 installs put the tray LaunchAgent here system-wide. Clean
# up the legacy location too.
rm -f /Library/LaunchAgents/com.gitopolis.konstantin-tray.plist

echo "→ removing binaries"
rm -f /usr/local/libexec/screentimed
rm -f /usr/local/bin/konstantin-status
rm -f /usr/local/bin/konstantin-tray

echo "→ removing socket if any"
rm -f /var/run/screentimed.sock

echo "→ removing bundle-watcher marker if any"
rm -f /etc/screentimed/bundle_path

# --- per-user tray LaunchAgent --------------------------------------
echo "→ removing per-user tray LaunchAgent(s)"
TRAY_USERS=("$@")
if [[ ${#TRAY_USERS[@]} -eq 0 ]]; then
    if [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
        TRAY_USERS=("${SUDO_USER}")
        echo "  no users passed; defaulting to \$SUDO_USER (${SUDO_USER})"
    else
        echo "  no users passed; skipping tray cleanup"
    fi
fi
for user in "${TRAY_USERS[@]}"; do
    uninstall_tray_for_user "$user"
done

cat <<EOF

done. system-side cleanup complete.

preserved on disk (remove manually if you really want them gone):
    /etc/screentimed/      (config)
    /var/db/screentimed/   (counter state)
EOF
