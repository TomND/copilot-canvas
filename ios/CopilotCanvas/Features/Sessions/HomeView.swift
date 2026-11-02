import SwiftUI

/// What a place's navigation stack shows on top of its list.
enum Route: Hashable {
    case terminal(String)
    /// A notebook folder (path, title).
    case folder(String, String)
    /// A note (path); `edit`: its source editor opens at once (a new note).
    case note(String, edit: Bool = false)
    case todo(String)
}

/// The places of a machine, like the desktop's spaces: the bottom tab bar.
enum Place: String, CaseIterable {
    case sessions, canvas, notebook, tasks

    var title: String {
        switch self {
        case .sessions: return "Sessions"
        case .canvas: return "Canvas"
        case .notebook: return "Notebook"
        case .tasks: return "Tasks"
        }
    }

    var symbol: String {
        switch self {
        case .sessions: return "list.bullet"
        case .canvas: return "rectangle.3.group"
        case .notebook: return "book.closed"
        case .tasks: return "checklist"
        }
    }

    /// What + makes here.
    var plusLabel: String {
        switch self {
        case .sessions, .canvas: return "New session"
        case .notebook: return "New note or folder"
        case .tasks: return "New todo"
        }
    }
}

/// What the notebook's + asks for.
enum NoteCreate: String, Identifiable {
    case note, folder
    var id: String { rawValue }
}

/// The current machine: Sessions, Canvas, Notebook and Tasks in a bottom
/// tab bar (each with its own navigation stack), under the machine's colour,
/// the connection banner and the attention strip.
struct HomeView: View {
    @ObservedObject var connection: ServiceConnection
    @EnvironmentObject private var model: AppModel
    @State private var place: Place = .sessions
    @State private var paths: [Place: [Route]] = [:]
    @State private var showNew = false
    @State private var showSettings = false
    @State private var showPair = false
    @State private var subsessionOf: SessionItem?
    @State private var showReports = false
    /// The report the inbox opens on (a notification).
    @State private var reportToOpen: String?
    @State private var noteCreate: NoteCreate?
    @State private var newTodo = false

    var body: some View {
        TabView(selection: $place) {
            ForEach(Place.allCases, id: \.self) { place in
                stack(place)
                    .tabItem { Label(place.title, systemImage: place.symbol) }
                    .badge(badge(place))
                    .tag(place)
            }
        }
        .overlay(alignment: .bottom) { ToastView(toast: $connection.toast).padding(.bottom, 64) }
        .sheet(isPresented: $showNew) {
            NewSessionView(connection: connection) { id in
                showNew = false
                open(.terminal(id))
            }
        }
        .sheet(isPresented: $showSettings) { SettingsView(connection: connection) }
        .sheet(isPresented: $showReports) {
            ReportsView(connection: connection, open: reportToOpen) { session in
                showReports = false
                openSession(session)
            }
        }
        .sheet(item: $subsessionOf) { parent in
            NewSessionView(connection: connection, parent: parent) { id in
                subsessionOf = nil
                open(.terminal(id))
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
        .sheet(isPresented: $newTodo) {
            NewTodoView(connection: connection) { id in
                newTodo = false
                paths[.tasks, default: []].append(.todo(id))
            }
        }
        // Naming a machine (Settings presents it itself while it is open).
        .sheet(item: Binding(get: { showSettings || showNew || showPair || showReports || newTodo || subsessionOf != nil || model.locked ? nil : model.naming }, set: { model.naming = $0 })) { server in
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
            // `-startMode canvas|notebook|tasks` / `-openSession ID`: automated simulator runs.
            let args = ProcessInfo.processInfo.arguments
            if let i = args.firstIndex(of: "-startMode"), i + 1 < args.count {
                place = Place(rawValue: args[i + 1].lowercased()) ?? (args[i + 1].lowercased() == "list" ? .sessions : place)
            }
            if let i = args.firstIndex(of: "-openSession"), i + 1 < args.count, paths[place, default: []].isEmpty { openSession(args[i + 1]) }
            if let i = args.firstIndex(of: "-openReport"), i + 1 < args.count { model.pendingReport = PendingReport(server: connection.server.id, report: args[i + 1]) }
        }
    }

    /// A place's navigation stack: the machine's colour, the connection
    /// banner and the attention strip above its list; the shared toolbar.
    private func stack(_ place: Place) -> some View {
        NavigationStack(path: Binding(get: { paths[place] ?? [] }, set: { paths[place] = $0 })) {
            VStack(spacing: 0) {
                // The machine's colour: always clear which one you are on.
                Rectangle().fill(Palette.group(UInt8(connection.server.colorIndex))).frame(height: 3)
                ConnectionBanner(connection: connection, repair: { showPair = true })
                AttentionStrip()
                content(place)
            }
            .background(Palette.background)
            .navigationTitle(place.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    MachineMenu(showPair: { showPair = true }, showSettings: { showSettings = true })
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    ReportsButton(unread: Int(connection.reports?.unread ?? 0)) {
                        reportToOpen = nil
                        showReports = true
                    }
                    plus(place)
                }
            }
            .navigationDestination(for: Route.self) { route in
                RouteView(connection: connection, route: route) { paths[place, default: []].append($0) }
            }
        }
    }

    @ViewBuilder
    private func content(_ place: Place) -> some View {
        let push: (Route) -> Void = { paths[place, default: []].append($0) }
        switch place {
        case .sessions:
            SessionsView(connection: connection, open: { push(.terminal($0)) }, subsession: { subsessionOf = $0 })
        case .canvas:
            CanvasView(connection: connection) { push(.terminal($0)) }
        case .notebook:
            NotebookHome(connection: connection, create: $noteCreate, push: push)
        case .tasks:
            TasksView(connection: connection, push: push)
        }
    }

    /// + makes what fits the place: a session, a note or folder, a todo.
    @ViewBuilder
    private func plus(_ place: Place) -> some View {
        switch place {
        case .sessions, .canvas:
            Button { showNew = true } label: { Image(systemName: "plus") }
                .disabled(connection.state != .online)
                .accessibilityLabel("New session")
                .accessibilityIdentifier("newSession")
        case .notebook:
            Menu {
                Button { noteCreate = .note } label: { Label("New note", systemImage: "doc.badge.plus") }
                Button { noteCreate = .folder } label: { Label("New folder", systemImage: "folder.badge.plus") }
            } label: { Image(systemName: "plus") }
                .disabled(connection.state != .online)
                .accessibilityLabel(place.plusLabel)
                .accessibilityIdentifier("newNoteMenu")
        case .tasks:
            Button { newTodo = true } label: { Image(systemName: "plus") }
                .disabled(connection.state != .online || connection.todos?.boards.isEmpty != false)
                .accessibilityLabel("New todo")
                .accessibilityIdentifier("newTodo")
        }
    }

    /// Sessions: those that need you here; Tasks: todos that wait for you.
    private func badge(_ place: Place) -> Int {
        switch place {
        case .sessions: return connection.overview?.sessions.filter { $0.kind == .session && $0.status == .needsYou }.count ?? 0
        case .tasks: return Set(connection.todos?.needsYou.map(\.todo.id) ?? []).count
        case .canvas, .notebook: return 0
        }
    }

    /// Opens a route in the place shown.
    private func open(_ route: Route) {
        paths[place, default: []].append(route)
    }

    /// A terminal opens in the Sessions or Canvas place (the one shown, else Sessions).
    private func openSession(_ id: String) {
        if place != .canvas { place = .sessions }
        paths[place] = [.terminal(id)]
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
        openSession(pending.session)
    }
}

/// What a route shows (in any place's stack).
struct RouteView: View {
    @ObservedObject var connection: ServiceConnection
    let route: Route
    var push: (Route) -> Void

    var body: some View {
        switch route {
        case .terminal(let id):
            // Viewing a terminal hides the tab bar: the keyboard and the screen need the room.
            TerminalScreen(connection: connection, sessionId: id)
                .toolbar(.hidden, for: .tabBar)
        case .folder(let path, let title):
            NoteFolderView(connection: connection, folder: path, title: title, push: push)
        case .note(let path, let edit):
            NoteView(connection: connection, path: path, editOnOpen: edit, push: push)
        case .todo(let id):
            TodoDetailView(connection: connection, id: id, push: push)
        }
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
