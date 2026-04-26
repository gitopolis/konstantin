#!/usr/bin/env bash
# Create local test accounts 'alice' and 'bob' for screentime testing.
#
# These are real macOS users. They get unique UIDs in the 600+ range so they
# don't collide with system or normal user UIDs.
#
# Run with: sudo ./packaging/create-test-users.sh
#
# Tear-down: ./packaging/delete-test-users.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (use sudo)" >&2
    exit 1
fi

create_user() {
    local name="$1"
    local uid="$2"
    local realname="$3"

    if id -u "${name}" >/dev/null 2>&1; then
        echo "→ user ${name} already exists, skipping"
        return
    fi

    echo "→ creating ${name} (uid ${uid})"
    dscl . -create "/Users/${name}"
    dscl . -create "/Users/${name}" UserShell /bin/zsh
    dscl . -create "/Users/${name}" RealName  "${realname}"
    dscl . -create "/Users/${name}" UniqueID  "${uid}"
    dscl . -create "/Users/${name}" PrimaryGroupID 20  # 'staff'
    dscl . -create "/Users/${name}" NFSHomeDirectory "/Users/${name}"
    dscl . -passwd "/Users/${name}" "screentime-test-${name}"
    install -d -m 0700 -o "${name}" -g staff "/Users/${name}"

    # Make sure the account is visible at the login window (it is by default
    # for UID >= 500, but explicit > implicit).
    dscl . -append /Groups/staff GroupMembership "${name}" || true

    echo "  ${name} ready. password: screentime-test-${name}"
}

create_user alice 601 "ScreenTime Test Alice"
create_user bob   602 "ScreenTime Test Bob"

cat <<EOF

created two test accounts:
    alice  (uid 601)  password: screentime-test-alice
    bob    (uid 602)  password: screentime-test-bob

these are full macOS user accounts. you can fast-user-switch into them from
the menu bar (Apple menu → Log Out → switch user) or, if hidden, from the
login window. delete them later with packaging/delete-test-users.sh.
EOF
