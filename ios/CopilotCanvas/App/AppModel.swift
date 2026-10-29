import Combine
import Foundation
import LocalAuthentication
import SwiftUI
import UIKit

/// A session to open on a given machine (a tapped notification or attention chip).
struct PendingOpen: Equatable {
    let server: UUID
    let session: String
}

/// A session of another machine that wants you (the attention strip).
struct AttentionItem: Identifiable, Equatable {
    var id: String { "\(server.id.uuidString)/\(session.id)" }
    let server: PairedServer
    let session: SessionItem
}

/// The app's state: paired machines, one live connection to each, which one
/// is shown, and the lock.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var servers: [PairedServer]
    /// One connection per paired machine, all live while the app is open.
    @Published private(set) var connections: [UUID: ServiceConnection] = [:]
    /// The machine shown.
    @Published private(set) var currentID: UUID?
    @Published var locked: Bool
    @Published var lockError: String?
    /// A pairing link opened from outside (the camera app, a `ccanvas://` URL).
    @Published var incomingLink: PairLinkInfo?
    /// A session to open, maybe on another machine.
    @Published var pendingOpen: PendingOpen?
    /// A machine to name and colour (just paired, or "Edit" in Settings).
    @Published var naming: PairedServer?

    private let store = ServerStore.shared
    private var backgroundedAt: Date?
    private var forwarding: [UUID: AnyCancellable] = [:]

    var lockEnabled: Bool {
        get { UserDefaults.standard.object(forKey: "lockWithFaceID") as? Bool ?? true }
        set {
            UserDefaults.standard.set(newValue, forKey: "lockWithFaceID")
            objectWillChange.send()
        }
    }

    var connection: ServiceConnection? { currentID.flatMap { connections[$0] } }
    var current: PairedServer? { connection?.server }

    init() {
        let servers = ServerStore.shared.load()
        self.servers = servers
        let lock = UserDefaults.standard.object(forKey: "lockWithFaceID") as? Bool ?? true
        // `-noLock`: UI tests on a simulator without a passcode prompt.
        locked = lock && !servers.isEmpty && !ProcessInfo.processInfo.arguments.contains("-noLock")
        for server in servers {
            makeConnection(server)
        }
        let saved = UserDefaults.standard.string(forKey: "currentServer").flatMap(UUID.init(uuidString:))
        currentID = servers.first(where: { $0.id == saved })?.id ?? servers.first?.id
        pairFromArguments()
    }

    // MARK: Machines

    /// Adds a newly paired machine, or updates the one at the same address
    /// (re-pairing keeps its name and colour). `ask`: offer to name it.
    func add(_ paired: PairedServer, ask: Bool = true) {
        var server = paired
        let isNew: Bool
        if let index = servers.firstIndex(where: { Self.sameAddress($0.url, server.url) }) {
            let old = servers[index]
            server.id = old.id
            server.name = old.name
            server.color = old.color
            servers[index] = server
            isNew = false
        } else {
            server.color = nextColor()
            servers.append(server)
            isNew = true
        }
        let failure = store.save(servers)
        makeConnection(server)
        select(server.id)
        if !locked { connection?.activate() }
        if let failure { connection?.toast = Toast(text: failure, error: true) }
        // Once the pairing sheet is gone: a name and a colour for the new machine.
        if isNew && ask {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.7) { [weak self] in self?.naming = server }
        }
    }

    func select(_ id: UUID) {
        guard connections[id] != nil else { return }
        currentID = id
        UserDefaults.standard.set(id.uuidString, forKey: "currentServer")
        if !locked { connections[id]?.activate() }
    }

    func forget(_ id: UUID) {
        servers.removeAll { $0.id == id }
        store.save(servers)
        connections[id]?.client.detach()
        connections[id] = nil
        forwarding[id] = nil
        if currentID == id {
            currentID = servers.first?.id
        }
    }

    func rename(_ id: UUID, to name: String, color: Int) {
        guard let index = servers.firstIndex(where: { $0.id == id }) else { return }
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { servers[index].name = String(trimmed.prefix(40)) }
        servers[index].color = color
        store.save(servers)
        connections[id]?.updateServer(servers[index])
    }

    func move(from source: IndexSet, to destination: Int) {
        servers.move(fromOffsets: source, toOffset: destination)
        store.save(servers)
    }

    /// Sessions of the other machines that need you or finished a turn.
    var attention: [AttentionItem] {
        servers.filter { $0.id != currentID }.flatMap { server -> [AttentionItem] in
            let sessions = connections[server.id]?.overview?.sessions ?? []
            return sessions
                .filter { $0.kind == .session && $0.status.wantsAttention }
                .sorted { ($0.status.rank, UInt64.max - $0.lastActivityMs) < ($1.status.rank, UInt64.max - $1.lastActivityMs) }
                .map { AttentionItem(server: server, session: $0) }
        }
    }

    /// Sessions of a machine that need you (its badge in the switcher).
    func needsYou(_ id: UUID) -> Int {
        connections[id]?.overview?.sessions.filter { $0.kind == .session && $0.status == .needsYou }.count ?? 0
    }

    /// Switches to `server` and opens `session` there.
    func open(session: String, on server: UUID) {
        guard connections[server] != nil else { return }
        select(server)
        pendingOpen = PendingOpen(server: server, session: session)
    }

    private func makeConnection(_ server: PairedServer) {
        connections[server.id]?.client.detach()
        let connection = ServiceConnection(server: server)
        connection.onServerChanged = { [weak self] updated in self?.update(updated) }
        connection.onAttention = { [weak self] connection, id, name, status in self?.attention(from: connection, id: id, name: name, status: status) }
        connections[server.id] = connection
        // Badges and the attention strip follow every machine.
        forwarding[server.id] = connection.objectWillChange.sink { [weak self] _ in self?.objectWillChange.send() }
    }

    /// A fresh tunnel token (or address) for a paired machine.
    private func update(_ server: PairedServer) {
        guard let index = servers.firstIndex(where: { $0.id == server.id }) else { return }
        servers[index] = server
        store.save(servers)
    }

    private func nextColor() -> Int {
        // Machines from before colours count as the first colour.
        let used = Set(servers.map(\.colorIndex))
        return (0..<Palette.groups.count).first { !used.contains($0) } ?? servers.count % Palette.groups.count
    }

    static func sameAddress(_ a: String, _ b: String) -> Bool {
        let normalize = { (url: String) in url.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "/ ")) }
        return normalize(a) == normalize(b)
    }

    /// A session of any machine needs you or finished: a toast on the screen
    /// shown (with the machine's name when it is another one), else a notification.
    private func attention(from connection: ServiceConnection, id: String, name: String, status: StatusKind) {
        let other = connection.server.id != currentID
        let title = servers.count > 1 ? "\(connection.server.name) · \(name)" : name
        let what = status == .needsYou ? "needs you" : "finished — ready for review"
        if UIApplication.shared.applicationState == .active && !locked {
            (self.connection ?? connection).toast = Toast(text: "\(title) \(what)", error: false)
            UINotificationFeedbackGenerator().notificationOccurred(status == .needsYou ? .warning : .success)
        } else {
            connection.notify(title: title, body: "The session \(what).", id: id)
        }
        if other { objectWillChange.send() }
    }

    // MARK: Lifecycle

    private func activateAll() {
        connections.values.forEach { $0.activate() }
    }

    func phaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .active:
            // Locked again after a minute away.
            if lockEnabled, !servers.isEmpty, let away = backgroundedAt, Date().timeIntervalSince(away) > 60 {
                locked = true
            }
            backgroundedAt = nil
            if !locked { activateAll() }
        case .background:
            backgroundedAt = Date()
            connections.values.forEach { $0.deactivate() }
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
            activateAll()
            return
        }
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: "Unlock the canvas of your Cloud PC") { ok, failure in
            DispatchQueue.main.async {
                if ok {
                    self.locked = false
                    self.lockError = nil
                    self.activateAll()
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

    /// `-pairLink URI` (repeatable) pairs machines not paired yet and
    /// `-machine NAME` shows one: automated simulator runs.
    private func pairFromArguments() {
        let args = ProcessInfo.processInfo.arguments
        let links = args.indices.filter { args[$0] == "-pairLink" && $0 + 1 < args.count }.compactMap { try? parsePairLink(uri: args[$0 + 1]) }
        let machine = args.firstIndex(of: "-machine").flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
        guard !links.isEmpty || machine != nil else { return }
        Task { @MainActor in
            for link in links where !servers.contains(where: { Self.sameAddress($0.url, link.url) }) {
                if let server = try? await Pairer.pair(link: link) {
                    add(server, ask: false)
                }
            }
            if let machine, let server = servers.first(where: { $0.name == machine }) {
                select(server.id)
            }
        }
    }
}
