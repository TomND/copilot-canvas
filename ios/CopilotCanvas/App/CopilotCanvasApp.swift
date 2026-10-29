import SwiftUI
import UserNotifications

@main
struct CopilotCanvasApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .preferredColorScheme(.dark)
                .tint(Palette.accent)
                .onOpenURL { model.handle(url: $0) }
                .onReceive(NotificationCenter.default.publisher(for: .openSession)) { note in
                    guard let info = note.object as? [String: String], let session = info["session"], !session.isEmpty else { return }
                    // Local notifications name the machine; pushes name this phone's client id there.
                    let server = info["server"].flatMap(UUID.init(uuidString:))
                        ?? info["client"].flatMap { client in model.servers.first { $0.clientId == client }?.id }
                        ?? model.currentID
                    if let server { model.open(session: session, on: server) }
                }
                .onReceive(NotificationCenter.default.publisher(for: .pushToken)) { note in
                    if let token = note.object as? String { model.pushToken = token }
                }
                .onReceive(NotificationCenter.default.publisher(for: .tunnelAccess)) { note in
                    if let info = note.object as? [String: String] { model.receiveAccess(info) }
                }
        }
        .onChange(of: phase) { _, newPhase in model.phaseChanged(newPhase) }
    }
}

extension Notification.Name {
    static let openSession = Notification.Name("copilot-canvas.open-session")
}

/// Notifications show while the app is open too; tapping one opens its session.
final class AppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        return true
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        // The machine it came from opens, then the session.
        var target: [String: String] = [:]
        if let session = info["session"] as? String {
            target = ["session": session, "server": info["server"] as? String ?? ""]
        } else if let cc = info["cc"] as? [String: Any], let session = cc["s"] as? String {
            target = ["session": session, "client": cc["c"] as? String ?? ""]
        }
        if !target.isEmpty {
            let object = target
            await MainActor.run { NotificationCenter.default.post(name: .openSession, object: object) }
        }
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        NotificationCenter.default.post(name: .pushToken, object: PushRegistration.hex(deviceToken))
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        NSLog("copilot-canvas: no push notifications: \(error.localizedDescription)")
    }

    /// A silent notification: a renewed tunnel token for one machine.
    func application(_ application: UIApplication, didReceiveRemoteNotification userInfo: [AnyHashable: Any]) async -> UIBackgroundFetchResult {
        guard let cc = userInfo["cc"] as? [String: Any], let client = cc["c"] as? String, let nonce = cc["n"] as? String, let sealed = cc["t"] as? String else { return .noData }
        let info = ["c": client, "n": nonce, "t": sealed]
        await MainActor.run { NotificationCenter.default.post(name: .tunnelAccess, object: info) }
        return .newData
    }
}

/// Pairing, the lock, or the canvas of the current service.
struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        ZStack {
            Palette.background.ignoresSafeArea()
            // A tap outside text inputs closes the keyboard, app-wide.
            KeyboardDismissInstaller().frame(width: 0, height: 0)
            if let connection = model.connection {
                // A new connection (another machine, or this one paired again)
                // is a new screen: nothing keeps talking to the old one.
                HomeView(connection: connection)
                    .id(ObjectIdentifier(connection))
            } else {
                PairView(initial: model.incomingLink) { server in model.add(server) }
            }
            if model.locked {
                LockView()
            }
        }
        .sheet(item: Binding(get: { model.connection == nil ? nil : model.incomingLink }, set: { model.incomingLink = $0 })) { link in
            NavigationStack {
                PairView(initial: link) { server in
                    model.incomingLink = nil
                    model.add(server)
                }
            }
        }

    }
}

extension PairLinkInfo: Identifiable {
    public var id: String { url + code }
}

/// Face ID / passcode before anything of the Cloud PC shows.
struct LockView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        ZStack {
            Palette.background.ignoresSafeArea()
            VStack(spacing: 18) {
                Image(systemName: "lock.fill").font(.system(size: 44)).foregroundStyle(Palette.accent)
                Text("Copilot Canvas is locked").font(.title3.weight(.semibold))
                Text("A paired phone can run anything on your Cloud PC.").font(.footnote).foregroundStyle(.secondary)
                Button {
                    model.unlock()
                } label: {
                    Label("Unlock", systemImage: "faceid").frame(maxWidth: 220)
                }
                .buttonStyle(.borderedProminent)
                if let error = model.lockError {
                    Text(error).font(.footnote).foregroundStyle(Palette.failed)
                }
            }
            .padding()
        }
        .onAppear { model.unlock() }
    }
}
