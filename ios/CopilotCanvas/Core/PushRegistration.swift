import Foundation
import UIKit
import UserNotifications

/// Notifications while the app is away (docs/IOS-PLAN.md, phase 5): the
/// phone registers with APNs and gives its device token to every paired
/// machine; a machine pushes when a session needs you or finished a turn,
/// and pushes renewed tunnel tokens (sealed with this phone's pairing
/// secret) so the phone can still connect after a day away.
enum PushRegistration {
    /// The APNs service this build's token belongs to: development-signed
    /// builds (Xcode) use the sandbox, App Store and TestFlight builds production.
    static let environment: String = {
        guard let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
              let data = try? Data(contentsOf: url),
              let text = String(data: data, encoding: .isoLatin1)
        else { return "production" }
        // The profile is a signed plist; its entitlements name the environment.
        guard let range = text.range(of: "<key>aps-environment</key>") else { return "production" }
        let rest = text[range.upperBound...].prefix(80)
        return rest.contains("development") ? "sandbox" : "production"
    }()

    static var bundle: String { Bundle.main.bundleIdentifier ?? "com.thomashorga.copilotcanvas" }

    /// Asks for permission (once) and registers with APNs; the token arrives
    /// in the app delegate.
    static func request() {
        guard !ProcessInfo.processInfo.arguments.contains("-noNotificationPrompt") else { return }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
            guard granted else { return }
            DispatchQueue.main.async { UIApplication.shared.registerForRemoteNotifications() }
        }
    }

    static func hex(_ token: Data) -> String {
        token.map { String(format: "%02x", $0) }.joined()
    }
}

extension Notification.Name {
    /// The APNs device token (hex) arrived.
    static let pushToken = Notification.Name("copilot-canvas.push-token")
    /// A silent notification with a sealed tunnel token: ["c", "n", "t"].
    static let tunnelAccess = Notification.Name("copilot-canvas.tunnel-access")
}
