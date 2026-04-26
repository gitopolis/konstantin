#!/usr/bin/env bash
# Remove the alice/bob test accounts created by create-test-users.sh.
# Safe to run repeatedly. Will refuse to delete a user that is currently
# logged in.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

delete_user() {
    local name="$1"
    if ! id -u "${name}" >/dev/null 2>&1; then
        echo "→ ${name} does not exist, skipping"
        return
    fi
    if who | awk '{print $1}' | grep -qx "${name}"; then
        echo "→ ${name} is currently logged in — log them out first" >&2
        return 1
    fi

    echo "→ deleting ${name}"
    dscl . -delete "/Users/${name}" || true
    rm -rf "/Users/${name}"
}

delete_user alice
delete_user bob

echo "done."
