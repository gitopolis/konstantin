# Homebrew cask formula for Screentime.
#
# This is a *reference* / starter file. Real distribution publishes it
# to a tap repo (e.g. github.com/qnicks/homebrew-screentime) so users
# can do:
#
#     brew tap qnicks/screentime
#     brew install --cask screentime
#
# Until a Developer ID is in place the bundle is unsigned. Cask handles
# the `com.apple.quarantine` attribute on install — no separate
# right-click-Open dance required.
#
# Bump `version` on each release. The `sha256` should match the SHA-256
# of the released `Screentime-<version>.zip` artifact (current
# `:no_check` is acceptable while the release pipeline is provisional;
# replace with a real digest once a release exists).

cask "screentime" do
  version "0.1.0"
  sha256 :no_check

  url "https://github.com/qnicks/screentime/releases/download/v#{version}/Screentime-#{version}.zip"
  name "Screentime"
  desc "macOS screen-time enforcer"
  homepage "https://github.com/qnicks/screentime"

  app "Screentime.app"

  # `brew uninstall screentime` runs this. Mirrors the in-app
  # `Uninstall…` menu item — stops the daemon, removes binaries +
  # plists. Preserves config and counter state at /etc/screentimed/
  # and /var/db/screentimed/.
  uninstall launchctl: "com.qnicks.screentimed",
            delete: [
              "/usr/local/libexec/screentimed",
              "/usr/local/bin/screentime-status",
              "/usr/local/bin/screentime-tray",
              "/Library/LaunchDaemons/com.qnicks.screentimed.plist",
              "/Library/LaunchAgents/com.qnicks.screentime-tray.plist",
              "/var/run/screentimed.sock",
            ]

  # `brew uninstall --zap screentime` runs this *in addition to*
  # `uninstall`. Removes everything — config, counter state, log file,
  # per-user LaunchAgent. For users who want a clean wipe.
  zap trash: [
        "~/Library/LaunchAgents/com.qnicks.screentime-tray.plist",
      ],
      delete: [
        "/etc/screentimed",
        "/var/db/screentimed",
        "/var/log/screentimed.log",
      ]
end
