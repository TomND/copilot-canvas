# Atelier for macOS (MAC-LINUX-PLAN §8.1): the app and the
# atelier command (service, CLI, TUI) on PATH.
#
# This folder is a Homebrew tap's layout. A release copies the cask that
# packaging/macos/build.sh writes (with the DMG's version and SHA-256) here
# and into the owner's tap repository; until the first release it checks
# nothing.
cask "atelier" do
  version "0.3.0"
  sha256 :no_check

  url "https://github.com/TomND/copilot-canvas/releases/download/v#{version}/Atelier-#{version}-macos-universal.dmg"
  name "Atelier"
  desc "Canvas of AI agent sessions and terminals that keep running"
  homepage "https://github.com/TomND/copilot-canvas"

  depends_on macos: ">= :ventura"

  app "Atelier.app"
  binary "#{appdir}/Atelier.app/Contents/MacOS/atelier"

  # Nothing is stopped on uninstall: brew upgrade uninstalls too, and an
  # upgrade keeps the sessions running (the new build takes the service over
  # and resumes them). `atelier service uninstall` removes the login
  # agent; `brew uninstall --zap` removes it and every canvas.
  zap launchctl: "com.thomashorga.atelier.service",
      trash:     [
        "~/.atelier",
        "~/Library/Application Support/BackgroundAutomations",
        "~/Library/LaunchAgents/com.thomashorga.atelier.service.plist",
        "~/Library/Saved Application State/io.github.tomnd.Atelier.savedState",
      ]
end
