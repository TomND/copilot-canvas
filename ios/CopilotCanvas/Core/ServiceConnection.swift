import Foundation
import SwiftUI
import UIKit
import UserNotifications

/// Bridges the Rust core's events (background threads) to the main actor.
private final class EventBridge: Listener, @unchecked Sendable {
    weak var connection: ServiceConnection?

    func onEvent(event: CoreEvent) {
        DispatchQueue.main.async { [weak self] in
            self?.connection?.handle(event)
        }
    }
}

/// The transport its own callbacks refer to, without keeping it alive.
private final class WeakTransport: @unchecked Sendable {
    weak var value: WebSocketTransport?
}

/// A short message shown at the bottom of the screen.
struct Toast: Identifiable, Equatable {
    let id = UUID()
    let text: String
    let error: Bool
}

/// One paired service: its WebSocket (reconnected with backoff), the Rust
/// core's client, and what the screens show.
@MainActor
final class ServiceConnection: ObservableObject {
    @Published private(set) var server: PairedServer
    @Published private(set) var state: ConnState = .connecting
    @Published private(set) var message: String = ""
    @Published private(set) var overview: Overview?
    /// The reports inbox (nil until the first read; `loaded` once the service sent it).
    @Published private(set) var reports: ReportsOverview?
    @Published var toast: Toast?

    let client: Client
    private let bridge = EventBridge()
    private var transport: WebSocketTransport?
    private var attempts = 0
    private var retry: Task<Void, Never>?
    private var overviewPending = false
    private var reportsPending = false
    private var active = false
    private var background: UIBackgroundTaskIdentifier = .invalid
    /// Terminal views waiting for screen updates.
    private var screenWatchers: [String: [UUID: () -> Void]] = [:]
    /// Called when the server record changes (a fresh tunnel token).
    var onServerChanged: ((PairedServer) -> Void)?
    /// A session started waiting for you or finished a turn (the app decides
    /// how to tell you: toast, notification, the attention strip).
    var onAttention: ((ServiceConnection, String, String, StatusKind) -> Void)?
    /// New reports came in (id, title, who published it, how many more).
    var onReport: ((ServiceConnection, String, String, String, UInt32) -> Void)?

    init(server: PairedServer) {
        self.server = server
        let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "ios"
        let config = ClientConfig(clientName: "iphone (\(UIDevice.current.name))", remoteId: server.clientId, secret: server.secret, build: "ios-\(build)")
        client = Client(config: config, listener: bridge)
        bridge.connection = self
    }

    // MARK: Lifecycle

    /// The app is in the foreground: connect (again).
    func activate() {
        endBackgroundTask()
        active = true
        if transport == nil && state != .revoked {
            connect()
        }
    }

    /// The app went to the background: keep the connection a little (iOS
    /// allows ~30 s) so attention events still arrive, then close it.
    func deactivate() {
        active = false
        retry?.cancel()
        background = UIApplication.shared.beginBackgroundTask(withName: "canvas-linger") { [weak self] in
            Task { @MainActor in self?.suspend() }
        }
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 25_000_000_000)
            if let self, !self.active { self.suspend() }
        }
    }

    private func suspend() {
        guard !active else { return }
        client.detach()
        transport = nil
        endBackgroundTask()
    }

    private func endBackgroundTask() {
        if background != .invalid {
            UIApplication.shared.endBackgroundTask(background)
            background = .invalid
        }
    }

    func connect() {
        retry?.cancel()
        guard let url = URL(string: server.url) else {
            state = .offline
            message = "Invalid address \(server.url)"
            return
        }
        if server.tokenExpired {
            message = "The tunnel access token expired. Scan a new QR code (`copilot-canvas remote pair --qr` on the Cloud PC)."
        }
        state = .connecting
        let client = self.client
        let box = WeakTransport()
        let made = WebSocketTransport(
            url: url,
            token: server.token,
            onOpen: {
                if let transport = box.value { client.attach(transport: transport) }
            },
            onData: { data in client.receive(data: data) },
            onClose: { [weak self] reason in
                client.transportClosed(reason: reason)
                let ended = box.value
                DispatchQueue.main.async { self?.transportEnded(ended, reason: reason) }
            }
        )
        box.value = made
        transport = made
        made.start()
    }

    private func transportEnded(_ ended: WebSocketTransport?, reason: String) {
        guard ended === transport else { return }
        transport = nil
        if state != .revoked {
            state = .offline
            if !reason.isEmpty && reason != "closed" { message = reason }
        }
        scheduleReconnect()
    }

    /// 0.5 s, 1 s, then every 1.5 s (the desktop's backoff), slower after a minute of failures.
    private func scheduleReconnect() {
        guard active, state != .revoked else { return }
        attempts += 1
        let delay: Double = attempts == 1 ? 0.5 : attempts == 2 ? 1 : attempts < 40 ? 1.5 : 5
        retry = Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard let self, !Task.isCancelled, self.active, self.transport == nil else { return }
            self.connect()
        }
    }

    /// Reconnect now (pull to refresh, the banner's button).
    func reconnectNow() {
        attempts = 0
        client.detach()
        transport = nil
        connect()
    }

    // MARK: Events

    fileprivate func handle(_ event: CoreEvent) {
        switch event {
        case .connection(let newState, let text):
            state = newState
            if newState == .online {
                attempts = 0
                message = ""
                requestNotifications()
            } else if !text.isEmpty && text != "paused" {
                message = text
            }
            refreshOverview()
            refreshReports()
        case .overview:
            refreshOverview()
            // Report rows name boxes of the canvas.
            refreshReports()
        case .reports:
            refreshReports()
        case .reportArrived(let id, let title, let source, let more):
            if let onReport {
                onReport(self, id, title, source, more)
            } else {
                toast = Toast(text: "New report\(source.isEmpty ? "" : " from \(source)"): \(title)", error: false)
            }
        case .screen(let id):
            screenWatchers[id]?.values.forEach { $0() }
        case .notice(let text, let error):
            toast = Toast(text: text, error: error)
        case .clipboard(let text):
            UIPasteboard.general.string = text
            toast = Toast(text: "Copied from the terminal", error: false)
        case .notification(_, let title, let body):
            notify(title: title.isEmpty ? "Terminal" : title, body: body)
        case .attention(let id, let name, let status):
            if let onAttention {
                onAttention(self, id, name, status)
                return
            }
            let what = status == .needsYou ? "needs you" : "finished — ready for review"
            if UIApplication.shared.applicationState == .active {
                toast = Toast(text: "\(name) \(what)", error: false)
                UINotificationFeedbackGenerator().notificationOccurred(status == .needsYou ? .warning : .success)
            } else {
                notify(title: name, body: "The session \(what).", id: id)
            }
        case .tunnelAccess(let url, let token, let expiresMs):
            var updated = server
            updated.token = token
            updated.tokenExpiresMs = expiresMs
            if !url.isEmpty, url.hasPrefix("wss://") { updated.url = url }
            if updated != server {
                server = updated
                onServerChanged?(updated)
            }
        }
    }

    /// Overview updates come in bursts: read it at most every 50 ms.
    private func refreshOverview() {
        guard !overviewPending else { return }
        overviewPending = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            guard let self else { return }
            self.overviewPending = false
            self.overview = self.client.overview()
        }
    }

    /// Inbox updates come in bursts too.
    private func refreshReports() {
        guard !reportsPending else { return }
        reportsPending = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            guard let self else { return }
            self.reportsPending = false
            self.reports = self.client.reports()
        }
    }

    /// The machine was renamed or recoloured (same address and credentials).
    func updateServer(_ updated: PairedServer) {
        server = updated
    }

    // MARK: Terminals

    /// Calls `changed` whenever terminal `id` has a new screen; returns a token to stop.
    func watchScreen(_ id: String, changed: @escaping () -> Void) -> UUID {
        let token = UUID()
        screenWatchers[id, default: [:]][token] = changed
        return token
    }

    func unwatchScreen(_ id: String, token: UUID) {
        screenWatchers[id]?[token] = nil
    }

    /// Runs a blocking core call off the main thread.
    func perform<T>(_ work: @escaping (Client) throws -> T) async throws -> T {
        let client = self.client
        return try await Task.detached(priority: .userInitiated) { try work(client) }.value
    }

    func show(_ error: Error) {
        let text: String
        if case let CoreError.Failed(message) = error { text = message } else { text = error.localizedDescription }
        toast = Toast(text: text, error: true)
    }

    // MARK: Notifications

    private func requestNotifications() {
        // Local alerts, and notifications from the machines while the app is away.
        PushRegistration.request()
    }

    /// A local notification; tapping it opens session `id` (or report `report`) here.
    func notify(title: String, body: String, id: String? = nil, report: String? = nil) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        if let id { content.userInfo = ["session": id, "server": server.id.uuidString] }
        if let report { content.userInfo = ["report": report, "server": server.id.uuidString] }
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }
}
