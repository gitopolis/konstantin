#!/usr/bin/env bash
# Install the screentime menu-bar app as a per-user LaunchAgent.
#
# This script does NOT need sudo. It assumes:
#   * `sudo ./packaging/install.sh` has already placed the system-wide
#     binaries (including /usr/local/bin/screentime-tray) and the
#     daemon plist.
#   * You are running it as the user that should see the menu-bar item.
#
# What it does:
#   * Copies the LaunchAgent plist to ~/Library/LaunchAgents/.
#   * Bootstraps it into the user's Aqua (GUI) launchd domain so it
#     starts immediately AND on every future login.

set -euo pipefail

if [[ $EUID -eq 0 ]]; then
    echo "do NOT run this with sudo — it installs into your home directory" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLIST_SRC="${ROOT}/packaging/com.qnicks.screentime-tray.plist"
PLIST_DST="${HOME}/Library/LaunchAgents/com.qnicks.screentime-tray.plist"
LABEL="com.qnicks.screentime-tray"
DOMAIN="gui/$(id -u)"

if [[ ! -x /usr/local/bin/screentime-tray ]]; then
    echo "missing /usr/local/bin/screentime-tray — run 'sudo ./packaging/install.sh' first" >&2
    exit 1
fi
if [[ ! -f "${PLIST_SRC}" ]]; then
    echo "missing ${PLIST_SRC}" >&2
    exit 1
fi

echo "→ creating ~/Library/LaunchAgents/"
install -d -m 0755 "${HOME}/Library/LaunchAgents"

echo "→ unloading any previous instance"
launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true

echo "→ copying plist to ${PLIST_DST}"
install -m 0644 "${PLIST_SRC}" "${PLIST_DST}"

echo "→ bootstrapping into ${DOMAIN}"
launchctl bootstrap "${DOMAIN}" "${PLIST_DST}"
launchctl enable    "${DOMAIN}/${LABEL}" || true
launchctl kickstart "${DOMAIN}/${LABEL}" || true

cat <<EOF

installed. the menu-bar item should appear shortly.

logs (per launchd plist):
    tail -f /tmp/screentime-tray.err.log

reload after editing the plist:
    launchctl bootout    ${DOMAIN}/${LABEL}
    launchctl bootstrap  ${DOMAIN} ${PLIST_DST}

uninstall:
    ./packaging/uninstall-tray.sh
EOF
