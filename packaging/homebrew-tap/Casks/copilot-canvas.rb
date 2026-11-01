# Copilot Canvas for macOS (MAC-LINUX-PLAN §8.1): the app and the
# copilot-canvas command (service, CLI, TUI) on PATH.
#
# This folder is a Homebrew tap's layout. A release copies the cask that
# packaging/macos/build.sh writes (with the DMG's version and SHA-256) here
# and into the owner's tap repository; until the first release it checks
# nothing.
cask "copilot-canvas" do
  version "0.3.0"
  sha256 :no_check

  url "https://github.com/TomND/copilot-canvas/releases/download/v#{version}/CopilotCanvas-#{version}-macos-universal.dmg"
  name "Copilot Canvas"
  desc "Canvas of AI agent sessions and terminals that keep running"
  homepage "https://github.com/TomND/copilot-canvas"

  depends_on macos: ">= :ventura"

  app "Copilot Canvas.app"
  binary "#{appdir}/Copilot Canvas.app/Contents/MacOS/copilot-canvas"

  # Nothing is stopped on uninstall: brew upgrade uninstalls too, and an
  # upgrade keeps the sessions running (the new build takes the service over
  # and resumes them). `copilot-canvas service uninstall` removes the login
  # agent; `brew uninstall --zap` removes it and every canvas.
  zap launchctl: "com.copilot-canvas.service",
      trash:     [
        "~/.copilot-canvas",
        "~/Library/Application Support/BackgroundAutomations",
        "~/Library/LaunchAgents/com.copilot-canvas.service.plist",
        "~/Library/Saved Application State/io.github.tomnd.CopilotCanvas.savedState",
      ]
end
