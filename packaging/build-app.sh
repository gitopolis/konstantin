#!/usr/bin/env bash
# Build Screentime.app from the workspace's release binaries.
#
# Usage:
#     cargo build --release
#     ./packaging/build-app.sh
#
# Output: target/Screentime.app/
#
# The bundle is ad-hoc codesigned (`codesign -s -`) so it loads on Apple
# Silicon — the kernel rejects unsigned binaries since macOS 11. Ad-hoc
# signing does NOT grant Gatekeeper trust; for distribution outside
# Homebrew cask you'd still need Developer ID + notarization.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${ROOT}/target/release"
APP="${ROOT}/target/Screentime.app"

# Read version from the workspace Cargo.toml (single source of truth).
VERSION=$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' \
    "${ROOT}/Cargo.toml")
if [[ -z "${VERSION}" ]]; then
    echo "could not read version from Cargo.toml" >&2
    exit 1
fi

# Verify all three release binaries exist.
for bin in screentimed screentime-status screentime-tray; do
    if [[ ! -x "${TARGET}/${bin}" ]]; then
        echo "missing ${TARGET}/${bin} — run 'cargo build --release' first" >&2
        exit 1
    fi
done

echo "→ building Screentime.app v${VERSION}"

# Start clean.
rm -rf "${APP}"

# Bundle layout (per CLAUDE.md "App bundle architecture").
install -d "${APP}/Contents/MacOS"
install -d "${APP}/Contents/Resources"
install -d "${APP}/Contents/Library/LaunchDaemons"

# Main executable — the tray. CFBundleExecutable in Info.plist points
# at this name.
install -m 0755 "${TARGET}/screentime-tray"   "${APP}/Contents/MacOS/screentime-tray"

# Resources: the daemon binary + diagnostic CLI + the example config.
# At first-launch install (phase A2) the tray copies the daemon binary
# from here into /usr/local/libexec/.
install -m 0755 "${TARGET}/screentimed"        "${APP}/Contents/Resources/screentimed"
install -m 0755 "${TARGET}/screentime-status"  "${APP}/Contents/Resources/screentime-status"
install -m 0644 "${ROOT}/packaging/config.example.toml" \
    "${APP}/Contents/Resources/config.example.toml"

# LaunchDaemon plist. We hand-install (cp + bootstrap) at runtime, but
# placing it in Contents/Library/LaunchDaemons/ matches SMAppService's
# expected layout — easy migration if/when we have Developer ID.
install -m 0644 "${ROOT}/packaging/com.qnicks.screentimed.plist" \
    "${APP}/Contents/Library/LaunchDaemons/com.qnicks.screentimed.plist"

# Optional icon. If neither form is present we just don't ship an icon
# — macOS falls back to the generic app icon. Provide artwork later by
# dropping a `packaging/AppIcon.iconset/` (an Apple iconset directory)
# or a pre-baked `packaging/AppIcon.icns`.
if [[ -f "${ROOT}/packaging/AppIcon.icns" ]]; then
    install -m 0644 "${ROOT}/packaging/AppIcon.icns" \
        "${APP}/Contents/Resources/AppIcon.icns"
    echo "  using packaging/AppIcon.icns"
elif [[ -d "${ROOT}/packaging/AppIcon.iconset" ]]; then
    iconutil --convert icns "${ROOT}/packaging/AppIcon.iconset" \
        --output "${APP}/Contents/Resources/AppIcon.icns"
    echo "  generated AppIcon.icns from packaging/AppIcon.iconset/"
else
    echo "  no icon found (drop a packaging/AppIcon.iconset/ to add one)"
fi

# Info.plist. Generated inline so version + identifier stay in sync
# with Cargo.toml automatically.
cat > "${APP}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>screentime-tray</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.qnicks.screentime</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Screentime</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>© Nikita</string>
</dict>
</plist>
EOF

# Ad-hoc codesign. `--deep` walks nested binaries (the daemon and CLI
# under Resources/). `--force` overwrites any signature cargo's linker
# left behind so the bundle has consistent signing.
echo "→ ad-hoc codesigning"
codesign --force --deep --sign - "${APP}" >/dev/null

# Validate. `codesign --verify` failing is a build-stop bug — without a
# valid signature the bundle won't launch on Apple Silicon.
codesign --verify --deep --strict "${APP}"

echo "built ${APP}"
echo "  size: $(du -sh "${APP}" | awk '{print $1}')"
