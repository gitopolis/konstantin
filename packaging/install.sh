#!/usr/bin/env bash
# Install screentimed + tray client on the local machine.
#
# This script ASSUMES you have built the workspace already:
#     cargo build --release
#
# It does NOT touch any existing user account. Test users are created
# separately by `packaging/create-test-users.sh`.
#
# Run with: sudo ./packaging/install.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${ROOT}/target/release"

for bin in screentimed screentime-status screentime-tray; do
    if [[ ! -x "${TARGET}/${bin}" ]]; then
        echo "missing ${TARGET}/${bin} — run 'cargo build --release' first" >&2
        exit 1
    fi
done

echo "→ installing daemon binary"
install -m 0755 "${TARGET}/screentimed"        /usr/local/libexec/screentimed
echo "→ installing CLI client"
install -m 0755 "${TARGET}/screentime-status"  /usr/local/bin/screentime-status
echo "→ installing menu-bar app"
install -m 0755 "${TARGET}/screentime-tray"    /usr/local/bin/screentime-tray

echo "→ creating /etc/screentimed/"
install -d -m 0755 /etc/screentimed
if [[ ! -e /etc/screentimed/config.toml ]]; then
    install -m 0644 "${ROOT}/packaging/config.example.toml" /etc/screentimed/config.toml
    echo "  wrote /etc/screentimed/config.toml from example"
else
    echo "  /etc/screentimed/config.toml already exists, leaving it alone"
fi

echo "→ creating /var/db/screentimed/"
install -d -m 0700 /var/db/screentimed

echo "→ installing LaunchDaemon plist"
install -m 0644 "${ROOT}/packaging/com.qnicks.screentimed.plist" \
    /Library/LaunchDaemons/com.qnicks.screentimed.plist

echo "→ installing LaunchAgent plist (will load per-user on next login)"
install -m 0644 "${ROOT}/packaging/com.qnicks.screentime-tray.plist" \
    /Library/LaunchAgents/com.qnicks.screentime-tray.plist

echo "→ loading LaunchDaemon"
launchctl bootstrap system /Library/LaunchDaemons/com.qnicks.screentimed.plist || true
launchctl enable system/com.qnicks.screentimed || true
launchctl kickstart -k system/com.qnicks.screentimed || true

cat <<EOF

installed. tail the daemon log with:
    tail -f /var/log/screentimed.log

next steps:
  1. sudo packaging/create-test-users.sh    # makes 'alice' and 'bob'
  2. log in as alice (or bob) and run:
        screentime-status
     it should print stub status from the daemon.

config lives at /etc/screentimed/config.toml — keep enforcement = "log"
until you've verified the daemon is making the decisions you expect.
EOF
