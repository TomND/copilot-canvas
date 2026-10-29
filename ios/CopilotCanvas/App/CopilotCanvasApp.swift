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
                    if let id = note.object as? String { model.openSession = id }
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
        if let id = response.notification.request.content.userInfo["session"] as? String {
            await MainActor.run { NotificationCenter.default.post(name: .openSession, object: id) }
        }
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
                HomeView(connection: connection)
                    .id(connection.server.id)
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
                    model.add(server)
                    model.incomingLink = nil
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
