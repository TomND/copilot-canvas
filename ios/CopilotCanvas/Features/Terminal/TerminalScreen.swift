import SwiftUI
import UIKit

/// A session's terminal, full screen: the live screen, the keyboard with its
/// key bar, the prompt composer, phone-size or desktop-size (mirror) display.
struct TerminalScreen: View {
    @ObservedObject var connection: ServiceConnection
    let sessionId: String

    @AppStorage("terminalFontSize") private var fontSize: Double = 12.5
    @AppStorage("terminalMirror") private var mirror = false
    @State private var grid: (cols: Int, rows: Int) = (0, 0)
    @State private var started = false
    @State private var starting = false
    @State private var startError: String?
    @State private var composing = false
    @State private var draft = ""
    @State private var showStopConfirm = false
    @State private var title = ""
    @State private var keyboardRequest = 0
    /// The keyboard (with its key bar) is up: the quick bar steps aside.
    @State private var keyboardShown = false
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL

    private var item: SessionItem? { connection.overview?.sessions.first { $0.id == sessionId } }

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                TerminalRepresentable(
                    connection: connection,
                    sessionId: sessionId,
                    fontSize: CGFloat(fontSize),
                    mirror: mirror,
                    keyboardRequest: keyboardRequest,
                    onGrid: { cols, rows in gridChanged(cols, rows) },
                    onFontSize: { fontSize = Double($0) },
                    onCompose: { composing = true },
                    onTitle: { title = $0 },
                    onLink: { if let url = URL(string: $0) { openURL(url) } }
                )
                .background(Color(rgb: TerminalColors.background))
                if !(item?.alive ?? false) {
                    notRunning
                }
            }
            if !keyboardShown { quickBar }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { _ in keyboardShown = true }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in keyboardShown = false }
        .background(Color(rgb: TerminalColors.background))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 0) {
                    HStack(spacing: 5) {
                        if let item { StatusIcon(status: item.status).frame(width: 16) }
                        Text(item?.name ?? "Terminal").font(.subheadline.weight(.semibold)).lineLimit(1)
                    }
                    Text(subtitle).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            ToolbarItem(placement: .topBarTrailing) { menu }
        }
        .toolbarBackground(Palette.panel, for: .navigationBar)
        .sheet(isPresented: $composing) {
            ComposerSheet(text: $draft, sessionName: item?.name ?? "the session") { text, enter in
                if enter { connection.client.submit(id: sessionId, text: text) } else { connection.client.paste(id: sessionId, text: text) }
                draft = ""
                composing = false
            }
            .presentationDetents([.medium, .large])
        }
        .confirmationDialog("Stop \(item?.name ?? "this session")?", isPresented: $showStopConfirm, titleVisibility: .visible) {
            Button("Stop", role: .destructive) { connection.client.stopSession(id: sessionId) }
        } message: {
            Text("The agent exits gracefully; the conversation stays resumable.")
        }
        .onDisappear {
            connection.client.closeTerminal()
        }
        .onChange(of: connection.state) { _, state in
            // A new connection shows this terminal again.
            if state == .online, started { open() } else if state == .online { start() }
        }
        .onReceive(connection.$overview) { _ in
            // Opened before the canvas arrived (a notification, a link): start now.
            if !started { start() }
        }
        .onChange(of: mirror) { _, _ in
            guard started, grid.cols > 0 else { return }
            connection.client.setFit(id: sessionId, fit: !mirror, cols: UInt16(grid.cols), rows: UInt16(grid.rows))
        }
    }

    private var subtitle: String {
        if !title.isEmpty { return title }
        guard let item else { return "" }
        return item.statusLabel + (item.cwd.isEmpty ? "" : " · " + item.cwd)
    }

    private var menu: some View {
        Menu {
            Picker("Size", selection: $mirror) {
                Label("Fit to this phone", systemImage: "iphone").tag(false)
                Label("Mirror the desktop", systemImage: "desktopcomputer").tag(true)
            }
            Section {
                Button { fontSize = min(28, fontSize + 1) } label: { Label("Larger text", systemImage: "textformat.size.larger") }
                Button { fontSize = max(7, fontSize - 1) } label: { Label("Smaller text", systemImage: "textformat.size.smaller") }
            }
            Section {
                Button { composing = true } label: { Label("Write a prompt…", systemImage: "square.and.pencil") }
                Button { connection.client.scroll(id: sessionId, delta: Int32.max) } label: { Label("Scroll to the top", systemImage: "arrow.up.to.line") }
                Button { connection.client.scroll(id: sessionId, delta: Int32.min) } label: { Label("Scroll to the bottom", systemImage: "arrow.down.to.line") }
                if item?.status == .done {
                    Button { connection.client.markReviewed(id: sessionId) } label: { Label("Mark reviewed", systemImage: "checkmark") }
                }
            }
            if item?.alive ?? false {
                Button(role: .destructive) { showStopConfirm = true } label: { Label("Stop session", systemImage: "stop.fill") }
            }
        } label: {
            Image(systemName: "ellipsis.circle")
        }
    }

    /// Keys for a phone held in one hand: answer prompts without the keyboard.
    private var quickBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                quick("keyboard", label: "Keyboard") { keyboardRequest += 1 }
                quick("square.and.pencil", label: "Prompt") { composing = true }
                quickKey("esc", .esc)
                quickKey("↑", .up)
                quickKey("↓", .down)
                quickKey("⏎", .enter)
                quickKey("1", .char(text: "1"))
                quickKey("2", .char(text: "2"))
                quickKey("3", .char(text: "3"))
                quickKey("y", .char(text: "y"))
                quickKey("n", .char(text: "n"))
                quickKey("tab", .tab)
                quickKey("^C", .char(text: "c"), mods: 4)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
        }
        .background(Palette.panel)
        .overlay(alignment: .top) { Rectangle().fill(Palette.border).frame(height: 0.5) }
    }

    private func quick(_ symbol: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol).frame(minWidth: 26, minHeight: 22)
        }
        .buttonStyle(.bordered)
        .accessibilityLabel(label)
    }

    private func quickKey(_ title: String, _ key: KeyCode, mods: UInt8 = 0) -> some View {
        Button {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            connection.client.sendKey(id: sessionId, key: key, mods: mods)
        } label: {
            Text(title).font(.system(.callout, design: .monospaced).weight(.medium)).frame(minWidth: 26, minHeight: 22)
        }
        .buttonStyle(.bordered)
        .disabled(!(item?.alive ?? false))
    }

    @ViewBuilder
    private var notRunning: some View {
        if starting || (item?.status == .preparing) {
            VStack(spacing: 10) {
                ProgressView()
                Text("Starting \(item?.name ?? "the session")…").foregroundStyle(.secondary)
            }
        } else if let item, started, !item.alive {
            VStack(spacing: 12) {
                Image(systemName: item.status.symbol).font(.largeTitle).foregroundStyle(item.status.color)
                Text(item.statusLabel.capitalized).font(.headline)
                if let startError { Text(startError).font(.footnote).foregroundStyle(Palette.failed).multilineTextAlignment(.center) }
                if item.canOpen {
                    Button("Resume") {
                        started = false
                        start()
                    }
                    .buttonStyle(.borderedProminent)
                }
            }
            .padding(24)
            .background(Palette.panel.opacity(0.92), in: RoundedRectangle(cornerRadius: 14))
        } else if let startError {
            Text(startError).font(.footnote).foregroundStyle(Palette.failed).padding()
        }
    }

    // MARK: Size and start

    private func gridChanged(_ cols: Int, _ rows: Int) {
        let first = grid.cols == 0
        grid = (cols, rows)
        if first {
            start()
        } else if started && !mirror {
            connection.client.resizeTerminal(id: sessionId, cols: UInt16(cols), rows: UInt16(rows))
        }
    }

    /// Shows the terminal; starts (or resumes) the session first when it is not running.
    private func start() {
        // Needs the screen's size, the connection and the session's box.
        guard grid.cols > 0, !starting, !started, connection.state == .online, item != nil else { return }
        let (cols, rows) = (UInt16(grid.cols), UInt16(grid.rows))
        if item?.alive ?? false {
            started = true
            open()
            return
        }
        guard item?.canOpen ?? false else {
            started = true
            open()
            return
        }
        starting = true
        startError = nil
        Task {
            do {
                try await connection.perform { try $0.openSession(id: sessionId, cols: cols, rows: rows) }
                started = true
                open()
            } catch {
                startError = describe(error)
                started = true
            }
            starting = false
        }
    }

    private func open() {
        connection.client.openTerminal(id: sessionId, fit: !mirror, cols: UInt16(grid.cols), rows: UInt16(grid.rows))
    }
}

/// Bridges the UIKit terminal view and the connection's screen updates.
struct TerminalRepresentable: UIViewRepresentable {
    let connection: ServiceConnection
    let sessionId: String
    let fontSize: CGFloat
    let mirror: Bool
    let keyboardRequest: Int
    let onGrid: (Int, Int) -> Void
    let onFontSize: (CGFloat) -> Void
    let onCompose: () -> Void
    let onTitle: (String) -> Void
    let onLink: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(frame: .zero)
        view.fontSize = fontSize
        view.mirror = mirror
        let client = connection.client
        let id = sessionId
        view.onKey = { key, mods in client.sendKey(id: id, key: key, mods: mods) }
        view.onText = { text in client.sendText(id: id, text: text) }
        view.onPaste = { text in client.paste(id: id, text: text) }
        view.onWheel = { up, col, row in client.wheel(id: id, up: up, col: col, row: row) }
        view.onClick = { col, row in client.click(id: id, col: col, row: row) }
        view.onGridSize = { cols, rows in DispatchQueue.main.async { onGrid(cols, rows) } }
        view.onFontSize = onFontSize
        view.onCompose = onCompose
        view.onLink = onLink
        view.selectionText = { c0, r0, c1, r1 in client.selectionText(id: id, fromCol: c0, fromRow: r0, toCol: c1, toRow: r1) }
        view.onCopied = { [weak connection] in connection?.toast = Toast(text: "Copied", error: false) }
        let coordinator = context.coordinator
        coordinator.view = view
        coordinator.onTitle = onTitle
        coordinator.token = connection.watchScreen(id) { [weak coordinator] in coordinator?.screenChanged(client: client, id: id) }
        coordinator.connection = connection
        coordinator.id = id
        // Whatever is known already (a reopened terminal).
        if let screen = client.takeScreen(id: id, full: true) { view.apply(screen) }
        return view
    }

    func updateUIView(_ view: TerminalView, context: Context) {
        view.fontSize = fontSize
        view.mirror = mirror
        if context.coordinator.keyboardRequest != keyboardRequest {
            context.coordinator.keyboardRequest = keyboardRequest
            if view.isFirstResponder { view.resignFirstResponder() } else { view.becomeFirstResponder() }
        }
    }

    static func dismantleUIView(_ view: TerminalView, coordinator: Coordinator) {
        if let token = coordinator.token, let id = coordinator.id {
            coordinator.connection?.unwatchScreen(id, token: token)
        }
    }

    @MainActor
    final class Coordinator {
        weak var view: TerminalView?
        weak var connection: ServiceConnection?
        var token: UUID?
        var id: String?
        var onTitle: ((String) -> Void)?
        var keyboardRequest = 0
        private var pending = false
        private var lastTitle = ""

        /// Frames arrive up to 60 times a second: take the screen once per run loop turn.
        func screenChanged(client: Client, id: String) {
            guard !pending else { return }
            pending = true
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.pending = false
                guard let screen = client.takeScreen(id: id, full: false) else { return }
                self.view?.apply(screen)
                if screen.title != self.lastTitle {
                    self.lastTitle = screen.title
                    self.onTitle?(screen.title)
                }
            }
        }
    }
}

/// Writing a prompt with the iOS keyboard (autocorrect, dictation), then
/// sending it in one piece: pasted, then Enter.
struct ComposerSheet: View {
    @Binding var text: String
    let sessionName: String
    let send: (String, Bool) -> Void
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            VStack(spacing: 12) {
                TextEditor(text: $text)
                    .focused($focused)
                    .scrollContentBackground(.hidden)
                    .scrollDismissesKeyboard(.interactively)
                    .padding(8)
                    .background(Palette.panel, in: RoundedRectangle(cornerRadius: 10))
                    .font(.body)
                Text("Sent as one paste, then Enter. Long-press the microphone on the keyboard to dictate.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding()
            .navigationTitle("Prompt for \(sessionName)")
            .navigationBarTitleDisplayMode(.inline)
            .keyboardDoneButton()
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Paste only") { send(text, false) }.disabled(text.isEmpty)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button { send(text, true) } label: { Label("Send", systemImage: "paperplane.fill") }
                        .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .onAppear { focused = true }
        }
    }
}
