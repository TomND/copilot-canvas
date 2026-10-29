import Foundation
import LocalAuthentication
import SwiftUI

/// The app's state: paired services, the current connection, the lock.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var servers: [PairedServer]
    @Published private(set) var connection: ServiceConnection?
    @Published var locked: Bool
    @Published var lockError: String?
    /// A pairing link opened from outside (the camera app, a `ccanvas://` URL).
    @Published var incomingLink: PairLinkInfo?
    /// A session to open (a tapped notification).
    @Published var openSession: String?

    private let store = ServerStore.shared
    private var backgroundedAt: Date?

    var lockEnabled: Bool {
        get { UserDefaults.standard.object(forKey: "lockWithFaceID") as? Bool ?? true }
        set {
            UserDefaults.standard.set(newValue, forKey: "lockWithFaceID")
            objectWillChange.send()
        }
    }

    var current: PairedServer? { connection?.server }

    init() {
        let servers = ServerStore.shared.load()
        self.servers = servers
        let lock = UserDefaults.standard.object(forKey: "lockWithFaceID") as? Bool ?? true
        // `-noLock`: UI tests on a simulator without a passcode prompt.
        locked = lock && !servers.isEmpty && !ProcessInfo.processInfo.arguments.contains("-noLock")
        let saved = UserDefaults.standard.string(forKey: "currentServer").flatMap(UUID.init(uuidString:))
        if let server = servers.first(where: { $0.id == saved }) ?? servers.first {
            open(server)
        }
    }

    // MARK: Servers

    func add(_ server: PairedServer) {
        servers.removeAll { $0.name.caseInsensitiveCompare(server.name) == .orderedSame && $0.url == server.url }
        servers.append(server)
        let failure = store.save(servers)
        open(server)
        connection?.activate()
        if let failure { connection?.toast = Toast(text: failure, error: true) }
    }

    func select(_ id: UUID) {
        guard let server = servers.first(where: { $0.id == id }), server.id != current?.id else { return }
        open(server)
        connection?.activate()
    }

    func forget(_ id: UUID) {
        servers.removeAll { $0.id == id }
        store.save(servers)
        if current?.id == id {
            connection?.client.detach()
            connection = nil
            if let next = servers.first { open(next); connection?.activate() }
        }
    }

    private func open(_ server: PairedServer) {
        connection?.client.detach()
        let connection = ServiceConnection(server: server)
        connection.onServerChanged = { [weak self] updated in self?.update(updated) }
        self.connection = connection
        UserDefaults.standard.set(server.id.uuidString, forKey: "currentServer")
    }

    /// A fresh tunnel token (or address) for a paired service.
    private func update(_ server: PairedServer) {
        guard let index = servers.firstIndex(where: { $0.id == server.id }) else { return }
        servers[index] = server
        store.save(servers)
    }

    // MARK: Lifecycle

    func phaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .active:
            // Locked again after a minute away.
            if lockEnabled, !servers.isEmpty, let away = backgroundedAt, Date().timeIntervalSince(away) > 60 {
                locked = true
            }
            backgroundedAt = nil
            if !locked { connection?.activate() }
        case .background:
            backgroundedAt = Date()
            connection?.deactivate()
        default:
            break
        }
    }

    func unlock() {
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // No passcode on this device (the simulator): nothing to unlock with.
            locked = false
            connection?.activate()
            return
        }
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: "Unlock the canvas of your Cloud PC") { ok, failure in
            DispatchQueue.main.async {
                if ok {
                    self.locked = false
                    self.lockError = nil
                    self.connection?.activate()
                } else {
                    self.lockError = failure?.localizedDescription
                }
            }
        }
    }

    // MARK: Links

    func handle(url: URL) {
        if let link = try? parsePairLink(uri: url.absoluteString) {
            incomingLink = link
        }
    }
}
