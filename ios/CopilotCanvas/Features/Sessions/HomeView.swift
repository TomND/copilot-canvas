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

    enum Mode: String, CaseIterable {
        case list = "Sessions"
        case canvas = "Canvas"
    }

    var body: some View {
        NavigationStack(path: $path) {
            VStack(spacing: 0) {
                ConnectionBanner(connection: connection)
                switch mode {
                case .list:
                    SessionsView(connection: connection) { id in path.append(.terminal(id)) }
                case .canvas:
                    CanvasView(connection: connection) { id in path.append(.terminal(id)) }
                }
            }
            .background(Palette.background)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showSettings = true } label: { Image(systemName: "gearshape") }
                }
                ToolbarItem(placement: .principal) {
                    Picker("View", selection: $mode) {
                        ForEach(Mode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 210)
                }
                ToolbarItem(placement: .topBarTrailing) {
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
        .onChange(of: model.openSession) { _, id in
            guard let id else { return }
            model.openSession = nil
            path = [.terminal(id)]
        }
        .onAppear {
            // Nothing connects before the lock is passed.
            if !model.locked { connection.activate() }
            // `-startMode canvas` / `-openSession ID`: automated simulator runs.
            let args = ProcessInfo.processInfo.arguments
            if let i = args.firstIndex(of: "-startMode"), i + 1 < args.count, let start = Mode(rawValue: args[i + 1].capitalized) { mode = start }
            if let i = args.firstIndex(of: "-openSession"), i + 1 < args.count, path.isEmpty { path = [.terminal(args[i + 1])] }
        }
    }
}

/// Offline, connecting, revoked: what is going on and what to do.
struct ConnectionBanner: View {
    @ObservedObject var connection: ServiceConnection

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
                    if !connection.message.isEmpty {
                        Text(connection.message).font(.caption).foregroundStyle(.secondary).lineLimit(4)
                    }
                }
                Spacer()
                if connection.state == .offline {
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
