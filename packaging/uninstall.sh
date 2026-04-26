#!/usr/bin/env bash
# Remove screentimed and the tray launch agent. Leaves /etc/screentimed and
# /var/db/screentimed intact so you don't lose configs / state on a reinstall.
#
# Run with: sudo ./packaging/uninstall.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

echo "→ unloading LaunchDaemon"
launchctl bootout system/com.qnicks.screentimed 2>/dev/null || true

echo "→ removing plists"
rm -f /Library/LaunchDaemons/com.qnicks.screentimed.plist
rm -f /Library/LaunchAgents/com.qnicks.screentime-tray.plist

echo "→ removing binaries"
rm -f /usr/local/libexec/screentimed
rm -f /usr/local/bin/screentime-status

echo "→ removing socket if any"
rm -f /var/run/screentimed.sock

echo "done. config and state preserved at:"
echo "    /etc/screentimed/"
echo "    /var/db/screentimed/"
