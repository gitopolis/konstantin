#!/usr/bin/env bash
# Remove the per-user tray LaunchAgent for the invoking user.
# Does NOT touch the system-wide daemon — for that, use `sudo
# ./packaging/uninstall.sh`.

set -euo pipefail

if [[ $EUID -eq 0 ]]; then
    echo "do NOT run this with sudo — it operates on your own LaunchAgents" >&2
    exit 1
fi

LABEL="com.gitopolis.konstantin-tray"
DOMAIN="gui/$(id -u)"
PLIST="${HOME}/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist"

echo "→ unloading ${DOMAIN}/${LABEL}"
launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true

echo "→ removing ${PLIST}"
rm -f "${PLIST}"

echo "done. system daemon untouched."
