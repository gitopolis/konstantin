# Homebrew cask formula for Konstantin.
#
# This is a *reference* / starter file. Real distribution publishes it
# to a tap repo (e.g. github.com/gitopolis/homebrew-konstantin) so users
# can do:
#
#     brew tap gitopolis/konstantin
#     brew install --cask konstantin
#
# Until a Developer ID is in place the bundle is unsigned. Cask handles
# the `com.apple.quarantine` attribute on install — no separate
# right-click-Open dance required.
#
# Bump `version` on each release. The `sha256` should match the SHA-256
# of the released `Konstantin-<version>.zip` artifact (current
# `:no_check` is acceptable while the release pipeline is provisional;
# replace with a real digest once a release exists).

cask "konstantin" do
  version "0.1.0"
  sha256 :no_check

  url "https://github.com/gitopolis/konstantin/releases/download/v#{version}/Konstantin-#{version}.zip"
  name "Konstantin"
  desc "macOS screen-time enforcer"
  homepage "https://github.com/gitopolis/konstantin"

  app "Konstantin.app"

  # `brew uninstall konstantin` runs this. Mirrors the in-app
  # `Uninstall…` menu item — stops the daemon, removes binaries +
  # plists. Preserves config and counter state at /etc/screentimed/
  # and /var/db/screentimed/.
  uninstall launchctl: "com.gitopolis.screentimed",
            delete: [
              "/usr/local/libexec/screentimed",
              "/usr/local/bin/konstantin-status",
              "/usr/local/bin/konstantin-tray",
              "/Library/LaunchDaemons/com.gitopolis.screentimed.plist",
              "/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist",
              "/var/run/screentimed.sock",
            ]

  # `brew uninstall --zap konstantin` runs this *in addition to*
  # `uninstall`. Removes everything — config, counter state, log file,
  # per-user LaunchAgent. For users who want a clean wipe.
  zap trash: [
        "~/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist",
      ],
      delete: [
        "/etc/screentimed",
        "/var/db/screentimed",
        "/var/log/screentimed.log",
      ]
end
