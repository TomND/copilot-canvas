import SwiftUI

enum Route: Hashable {
    case terminal(String)
}

/// The current service: the session list or the canvas, and terminals.
struct HomeView: View {
    @ObservedObject var connection: ServiceConnection
    @EnvironmentObject private var model: AppModel
    @State private var path: [Route] = []
    @State private var mode: Mode = .list
    @State private var showNew = false
    @State private var showSettings = false
    @State private var showPair = false
    @State private var subsessionOf: SessionItem?
    @State private var showReports = false
    /// The report the inbox opens on (a notification).
    @State private var reportToOpen: String?

    enum Mode: String, CaseIterable {
        case list = "Sessions"
        case canvas = "Canvas"

        var symbol: String { self == .list ? "list.bullet" : "rectangle.3.group" }
    }

    var body: some View {
        NavigationStack(path: $path) {
            VStack(spacing: 0) {
                // The machine's colour: always clear which one you are on.
                Rectangle().fill(Palette.group(UInt8(connection.server.colorIndex))).frame(height: 3)
                ConnectionBanner(connection: connection, repair: { showPair = true })
                AttentionStrip()
                switch mode {
                case .list:
                    SessionsView(connection: connection, open: { id in path.append(.terminal(id)) }, subsession: { subsessionOf = $0 })
                case .canvas:
                    CanvasView(connection: connection) { id in path.append(.terminal(id)) }
                }
            }
            .background(Palette.background)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    MachineMenu(showPair: { showPair = true }, showSettings: { showSettings = true })
                }
                ToolbarItem(placement: .principal) {
                    Picker("View", selection: $mode) {
                        ForEach(Mode.allCases, id: \.self) { mode in
                            Label(mode.rawValue, systemImage: mode.symbol).labelStyle(.iconOnly).tag(mode)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 104)
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    ReportsButton(unread: Int(connection.reports?.unread ?? 0)) {
                        reportToOpen = nil
                        showReports = true
                    }
                    Button { showNew = true } label: { Image(systemName: "plus") }
                        .disabled(connection.state != .online)
                        .accessibilityLabel("New session")
                }
            }
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .terminal(let id):
                    TerminalScreen(connection: connection, sessionId: id)
                }
            }
        }
        .overlay(alignment: .bottom) { ToastView(toast: $connection.toast).padding(.bottom, 24) }
        .sheet(isPresented: $showNew) {
            NewSessionView(connection: connection) { id in
                showNew = false
                path.append(.terminal(id))
            }
        }
        .sheet(isPresented: $showSettings) { SettingsView(connection: connection) }
        .sheet(isPresented: $showReports) {
            ReportsView(connection: connection, open: reportToOpen) { session in
                showReports = false
                path = [.terminal(session)]
            }
        }
        .sheet(item: $subsessionOf) { parent in
            NewSessionView(connection: connection, parent: parent) { id in
                subsessionOf = nil
                path.append(.terminal(id))
            }
        }
        .sheet(isPresented: $showPair) {
            NavigationStack {
                PairView { server in
                    showPair = false
                    model.add(server)
                }
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { showPair = false } } }
            }
        }
        // Naming a machine (Settings presents it itself while it is open).
        .sheet(item: Binding(get: { showSettings || showNew || showPair || showReports || subsessionOf != nil || model.locked ? nil : model.naming }, set: { model.naming = $0 })) { server in
            MachineSheet(server: server) { name, color in
                model.rename(server.id, to: name, color: color)
                model.naming = nil
            }
        }
        .onChange(of: model.pendingOpen) { _, _ in openPending() }
        .onChange(of: model.pendingReport) { _, _ in openPendingReport() }
        .onAppear {
            // Nothing connects before the lock is passed.
            if !model.locked { connection.activate() }
            openPending()
            openPendingReport()
            // `-startMode canvas` / `-openSession ID`: automated simulator runs.
            let args = ProcessInfo.processInfo.arguments
            if let i = args.firstIndex(of: "-startMode"), i + 1 < args.count, let start = Mode(rawValue: args[i + 1].capitalized) { mode = start }
            if let i = args.firstIndex(of: "-openSession"), i + 1 < args.count, path.isEmpty { path = [.terminal(args[i + 1])] }
            if let i = args.firstIndex(of: "-openReport"), i + 1 < args.count { model.pendingReport = PendingReport(server: connection.server.id, report: args[i + 1]) }
        }
    }

    /// A report to open on this machine (a notification).
    private func openPendingReport() {
        guard let pending = model.pendingReport, pending.server == connection.server.id else { return }
        model.pendingReport = nil
        showReports = false
        DispatchQueue.main.async {
            reportToOpen = pending.report
            showReports = true
        }
    }

    /// A session to open on this machine (a notification, an attention chip).
    private func openPending() {
        guard let pending = model.pendingOpen, pending.server == connection.server.id else { return }
        model.pendingOpen = nil
        path = [.terminal(pending.session)]
    }
}

/// The reports inbox, with how many rows are unread.
struct ReportsButton: View {
    let unread: Int
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: unread > 0 ? "tray.full.fill" : "tray")
                .overlay(alignment: .topTrailing) {
                    if unread > 0 {
                        Text(unread > 99 ? "99+" : "\(unread)")
                            .font(.system(size: 10, weight: .bold))
                            .padding(.horizontal, 4)
                            .padding(.vertical, 1)
                            .background(Palette.attention, in: Capsule())
                            .foregroundStyle(.black)
                            .offset(x: 9, y: -7)
                    }
                }
        }
        .accessibilityIdentifier("reportsButton")
        .accessibilityLabel(unread > 0 ? "Reports, \(unread) unread" : "Reports")
    }
}

/// Offline, connecting, revoked: what is going on and what to do.
struct ConnectionBanner: View {
    @ObservedObject var connection: ServiceConnection
    /// Pair this machine again (a new QR code: the tunnel access ran out).
    var repair: (() -> Void)?

    /// Offline because the tunnel no longer lets this phone in.
    private var accessExpired: Bool {
        connection.state != .online && (connection.server.tokenExpired || connection.message.contains("HTTP 401") || connection.message.contains("sign-in"))
    }

    var body: some View {
        if connection.state != .online {
            HStack(alignment: .top, spacing: 10) {
                switch connection.state {
                case .connecting:
                    ProgressView().controlSize(.small)
                case .revoked:
                    Image(systemName: "xmark.shield.fill").foregroundStyle(Palette.failed)
                default:
                    Image(systemName: "wifi.exclamationmark").foregroundStyle(Palette.attention)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(.footnote.weight(.semibold))
                    if accessExpired {
                        Text("Its tunnel access token expired while the app was away. On \(connection.server.name), run `copilot-canvas remote pair --qr` and scan the code: the pairing is replaced, the name and colour stay.")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(5)
                    } else if !connection.message.isEmpty {
                        Text(connection.message).font(.caption).foregroundStyle(.secondary).lineLimit(4)
                    }
                }
                Spacer()
                if accessExpired, let repair {
                    Button("Pair again", action: repair).font(.footnote.weight(.semibold))
                } else if connection.state == .offline {
                    Button("Retry") { connection.reconnectNow() }.font(.footnote)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Palette.panel)
            .overlay(alignment: .bottom) { Rectangle().fill(Palette.border).frame(height: 0.5) }
        }
    }

    private var title: String {
        if accessExpired { return "Tunnel access expired" }
        switch connection.state {
        case .connecting: return "Connecting to \(connection.server.name)…"
        case .offline: return "Offline — reconnecting…"
        case .revoked: return "This phone was revoked on the service"
        case .online: return ""
        }
    }
}

struct ToastView: View {
    @Binding var toast: Toast?

    var body: some View {
        if let toast {
            Label(toast.text, systemImage: toast.error ? "exclamationmark.triangle.fill" : "info.circle.fill")
                .font(.footnote)
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(.ultraThinMaterial, in: Capsule())
                .overlay(Capsule().stroke(toast.error ? Palette.failed.opacity(0.6) : Palette.border))
                .padding(.horizontal, 20)
                .transition(.move(edge: .bottom).combined(with: .opacity))
                .task(id: toast.id) {
                    try? await Task.sleep(nanoseconds: 3_000_000_000)
                    withAnimation { self.toast = nil }
                }
        }
    }
}
