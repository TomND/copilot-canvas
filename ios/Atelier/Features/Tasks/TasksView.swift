import SwiftUI

extension TodoItem: Identifiable {}
extension TodoBoardItem: Identifiable {}
extension TodoLane: Identifiable {}

extension TodoNeedItem: Identifiable {
    public var id: String { "\(need)-\(todo.id)" }
}

extension TodoStatus {
    static let all: [TodoStatus] = [.proposed, .backlog, .todo, .inProgress, .inReview, .done, .canceled]

    /// The default column names (boards may rename them: `TodoItem.statusLabel`).
    var label: String {
        switch self {
        case .proposed: return "Proposed"
        case .backlog: return "Backlog"
        case .todo: return "Todo"
        case .inProgress: return "In progress"
        case .inReview: return "In review"
        case .done: return "Done"
        case .canceled: return "Canceled"
        }
    }

    var color: Color {
        switch self {
        case .proposed: return Palette.attention
        case .backlog: return Palette.gray
        case .todo: return Palette.text
        case .inProgress: return Palette.positive
        case .inReview: return Palette.finished
        case .done: return Palette.loop
        case .canceled: return Palette.dark
        }
    }

    var closed: Bool { self == .done || self == .canceled }
}

extension TodoKind {
    static let all: [TodoKind] = [.bug, .feature, .improvement, .optimization, .chore, .docs, .research]

    var label: String {
        switch self {
        case .bug: return "Bug"
        case .feature: return "Feature"
        case .improvement: return "Improvement"
        case .optimization: return "Optimization"
        case .chore: return "Chore"
        case .docs: return "Docs"
        case .research: return "Research"
        }
    }

    var color: Color {
        switch self {
        case .bug: return Palette.failed
        case .feature: return Palette.positive
        case .improvement: return Palette.accent
        case .optimization: return Palette.loop
        case .chore: return Palette.gray
        case .docs: return Palette.purple
        case .research: return Palette.attention
        }
    }
}

extension TodoPriority {
    static let all: [TodoPriority] = [.urgent, .high, .medium, .low, .noPriority]

    var label: String {
        switch self {
        case .urgent: return "Urgent"
        case .high: return "High"
        case .medium: return "Medium"
        case .low: return "Low"
        case .noPriority: return "None"
        }
    }
}

extension TodoNeed {
    var label: String {
        switch self {
        case .proposed: return "proposal"
        case .inReview: return "review"
        case .staleClaim: return "claim gone"
        }
    }
}

/// The Tasks place: what waits for you first (proposals, reviews, claims of
/// sessions that are gone), then one board as a list by status, its
/// swimlanes as captions inside each status. Viewing never starts a session.
struct TasksView: View {
    @ObservedObject var connection: ServiceConnection
    var push: (Route) -> Void
    @AppStorage("todoBoard") private var boardId = ""
    @State private var showDone = false
    @State private var declining: TodoItem?
    @State private var reason = ""

    var body: some View {
        Group {
            if let todos = connection.todos, todos.loaded {
                if todos.boards.isEmpty {
                    ContentUnavailableView("No todo boards yet", systemImage: "checklist", description: Text("Create a board on the desktop (Tasks › + New board) or let an agent run `atelier todo board add`."))
                } else {
                    list(todos)
                }
            } else if connection.state == .online {
                VStack(spacing: 12) {
                    ProgressView()
                    Text("Waiting for the service's todo boards…").foregroundStyle(.secondary)
                    Text("An older Atelier service sends none: update it on \(connection.server.name).").font(.caption).foregroundStyle(Palette.muted).multilineTextAlignment(.center)
                }
                .padding()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView("Offline", systemImage: "wifi.exclamationmark", description: Text("The boards show once \(connection.server.name) is connected."))
            }
        }
        .onAppear { connection.refreshTodos() }
        .alert("Decline \(declining?.id ?? "")?", isPresented: Binding(get: { declining != nil }, set: { if !$0 { declining = nil } })) {
            TextField("Why (optional)", text: $reason)
            Button("Decline", role: .destructive) {
                if let item = declining { connection.todoChange { try $0.declineTodo(id: item.id, reason: reason) } }
                reason = ""
            }
            Button("Cancel", role: .cancel) { reason = "" }
        } message: {
            Text(declining?.title ?? "")
        }
    }

    private func list(_ todos: TodosOverview) -> some View {
        let items = Dictionary(todos.todos.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let board = todos.boards.first { $0.id == boardId } ?? todos.boards[0]
        return List {
            let waiting = needs(todos)
            if !waiting.isEmpty {
                SwiftUI.Section {
                    ForEach(waiting, id: \.item.id) { entry in
                        row(entry.item, needs: entry.needs)
                    }
                } header: {
                    HStack(spacing: 6) {
                        RoundedRectangle(cornerRadius: 2).fill(Palette.attention).frame(width: 8, height: 8)
                        Text("Needs you")
                        Spacer()
                        Text("\(waiting.count)").foregroundStyle(.secondary)
                    }
                }
            }
            SwiftUI.Section {
                Picker(selection: Binding(get: { board.id }, set: { boardId = $0 })) {
                    ForEach(todos.boards) { Text($0.name).tag($0.id) }
                } label: {
                    Label("Board", systemImage: "rectangle.split.3x1")
                }
                .pickerStyle(.menu)
                .accessibilityIdentifier("boardPicker")
                .listRowBackground(Palette.panel)
                Text(summary(board)).font(.caption).foregroundStyle(Palette.gray).listRowBackground(Palette.panel)
            }
            // Proposals are all in Needs you, above.
            ForEach(board.columns.filter { $0.count > 0 && ![TodoStatus.proposed, .done, .canceled].contains($0.status) }, id: \.status) { column in
                SwiftUI.Section {
                    cells(column, board: board, items: items)
                } header: {
                    columnHeader(column)
                }
            }
            if let done = board.columns.first(where: { $0.status == .done }), done.count > 0 {
                SwiftUI.Section {
                    if showDone { cells(done, board: board, items: items) }
                } header: {
                    HStack {
                        columnHeader(done)
                        Button(showDone ? "Hide" : "Show") { withAnimation { showDone.toggle() } }
                            .font(.caption.weight(.semibold))
                            .accessibilityIdentifier("showDone")
                    }
                }
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .accessibilityIdentifier("tasksList")
        .refreshable { connection.refreshTodos() }
    }

    /// What waits for you, one row per todo (in review with its session gone: both).
    private func needs(_ todos: TodosOverview) -> [(item: TodoItem, needs: [TodoNeed])] {
        var out: [(item: TodoItem, needs: [TodoNeed])] = []
        for need in todos.needsYou {
            if let index = out.firstIndex(where: { $0.item.id == need.todo.id }) {
                out[index].needs.append(need.need)
            } else {
                out.append((need.todo, [need.need]))
            }
        }
        return out
    }

    /// "3/7 done · 1 blocked · 1 canceled · agents' todos are proposals".
    private func summary(_ board: TodoBoardItem) -> String {
        var parts = ["\(board.done)/\(board.total) done"]
        if board.blocked > 0 { parts.append("\(board.blocked) blocked") }
        if board.canceled > 0 { parts.append("\(board.canceled) canceled") }
        if board.lanes.count > 1 { parts.append("\(board.lanes.count - 1) swimlane\(board.lanes.count == 2 ? "" : "s")") }
        if board.agentsPropose { parts.append("agents propose") }
        return parts.joined(separator: " · ")
    }

    private func columnHeader(_ column: TodoColumn) -> some View {
        HStack(spacing: 6) {
            Circle().fill(column.status.color).frame(width: 7, height: 7)
            Text(column.name)
            Spacer()
            if column.wip > 0 {
                Text("\(column.count)/\(column.wip)").foregroundStyle(column.over ? Palette.attention : .secondary)
            } else {
                Text("\(column.count)").foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private func cells(_ column: TodoColumn, board: TodoBoardItem, items: [String: TodoItem]) -> some View {
        ForEach(column.cells, id: \.lane) { cell in
            if board.lanes.count > 1 && !cell.ids.isEmpty {
                Text(board.lanes.first { $0.id == cell.lane }?.name ?? "")
                    .font(.caption2.weight(.semibold))
                    .textCase(.uppercase)
                    .foregroundStyle(Palette.muted)
                    .listRowBackground(Palette.background.opacity(0.4))
                    .accessibilityAddTraits(.isHeader)
            }
            ForEach(cell.ids, id: \.self) { id in
                if let item = items[id] { row(item, needs: []) }
            }
        }
    }

    private func row(_ item: TodoItem, needs: [TodoNeed]) -> some View {
        NavigationLink(value: Route.todo(item.id)) {
            TodoRow(item: item, needs: needs)
        }
        .listRowBackground(needs.isEmpty ? Palette.panel : Palette.attentionBg.opacity(0.55))
        .swipeActions(edge: .leading) {
            if item.status == .proposed {
                Button { connection.todoChange { try $0.acceptTodo(id: item.id, status: nil) } } label: { Label("Accept", systemImage: "checkmark") }.tint(Palette.finished)
            } else if !item.status.closed {
                Button { connection.todoChange { try $0.doneTodo(id: item.id) } } label: { Label("Done", systemImage: "checkmark.circle") }.tint(Palette.loop)
            }
        }
        .swipeActions(edge: .trailing) {
            if item.status == .proposed {
                Button { declining = item } label: { Label("Decline", systemImage: "xmark") }.tint(Palette.failed)
            } else if needs.contains(.staleClaim) {
                Button { connection.todoChange { try $0.releaseTodo(id: item.id) } } label: { Label("Release", systemImage: "person.crop.circle.badge.xmark") }.tint(Palette.attention)
            }
        }
        .contextMenu { TodoMenu(connection: connection, item: item, decline: { declining = item }) }
    }
}

/// The changes a todo's long-press offers (the desktop's, as you).
struct TodoMenu: View {
    @ObservedObject var connection: ServiceConnection
    let item: TodoItem
    var decline: () -> Void

    var body: some View {
        if item.status == .proposed {
            Menu {
                ForEach([TodoStatus.backlog, .todo, .inProgress], id: \.self) { status in
                    Button(status.label) { connection.todoChange { try $0.acceptTodo(id: item.id, status: status) } }
                }
            } label: { Label("Accept into…", systemImage: "checkmark") }
            Button(role: .destructive, action: decline) { Label("Decline…", systemImage: "xmark") }
        } else if !item.status.closed {
            Menu {
                ForEach([TodoStatus.backlog, .todo, .inProgress, .inReview].filter { $0 != item.status }, id: \.self) { status in
                    Button(status.label) { connection.todoChange { try $0.moveTodo(id: item.id, status: status) } }
                }
            } label: { Label("Move to…", systemImage: "arrow.right.circle") }
            Button { connection.todoChange { try $0.doneTodo(id: item.id) } } label: { Label("Done", systemImage: "checkmark.circle") }
        } else {
            Button { connection.todoChange { try $0.reopenTodo(id: item.id) } } label: { Label("Reopen", systemImage: "arrow.uturn.backward") }
        }
        if !item.claimedBy.isEmpty {
            Button { connection.todoChange { try $0.releaseTodo(id: item.id) } } label: { Label("Release the claim", systemImage: "person.crop.circle.badge.xmark") }
        }
        Button { UIPasteboard.general.string = item.id } label: { Label("Copy id", systemImage: "doc.on.doc") }
    }
}

/// A todo as a row: type, id, priority, blocker, title; then what matters
/// about it (blocked, checklist, the session working on it, labels).
struct TodoRow: View {
    let item: TodoItem
    /// Why it waits for you (the Needs you section).
    var needs: [TodoNeed] = []

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Text(item.kindGlyph)
                .font(.system(.body, design: .monospaced).weight(.bold))
                .foregroundStyle(item.kind.color)
                .frame(width: 18)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(item.id).font(.caption.monospaced()).foregroundStyle(Palette.muted)
                    if !item.priorityMarks.isEmpty {
                        Text(item.priorityMarks).font(.caption.weight(.heavy)).foregroundStyle(item.priority == .urgent ? Palette.failed : Palette.attention)
                    }
                    if item.blocker { Text("‼\u{FE0E}").font(.caption.weight(.heavy)).foregroundStyle(Palette.failed) }
                    if item.aiCreated { Text("◇").font(.caption).foregroundStyle(Palette.purple) }
                    Spacer(minLength: 4)
                    if !needs.isEmpty {
                        Text(needs.map(\.label).joined(separator: " · "))
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 6)
                            .padding(.vertical, 1)
                            .background(Palette.attention.opacity(0.2), in: Capsule())
                            .foregroundStyle(Palette.attention)
                    } else if item.claimWorking {
                        Image(systemName: "circle.dotted").font(.caption).foregroundStyle(Palette.positive)
                    }
                }
                Text(item.title)
                    .font(.body)
                    .foregroundStyle(item.status.closed ? Palette.gray : Palette.text)
                    .strikethrough(item.status == .canceled)
                    .lineLimit(2)
                let badges = self.badges
                if !badges.characters.isEmpty {
                    Text(badges).font(.caption).lineLimit(2)
                }
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("todoRow")
        .accessibilityLabel(accessibility)
    }

    /// "In review · ⚑ blocked: why · ☑ 2/4 · ⟳ Fix flaky test · ui".
    private var badges: AttributedString {
        var parts: [AttributedString] = []
        func part(_ text: String, _ color: Color) {
            var piece = AttributedString(text)
            piece.foregroundColor = color
            parts.append(piece)
        }
        if !needs.isEmpty { part(item.statusLabel, item.status.color) }
        if item.blocked { part(item.blockedReason.isEmpty ? "⚑ blocked" : "⚑ \(item.blockedReason)", Palette.failed) }
        if item.blocks > 0 { part("blocks \(item.blocks)", Palette.attention) }
        if item.checklistTotal > 0 { part("☑ \(item.checklistDone)/\(item.checklistTotal)", Palette.gray) }
        if !item.claimedBy.isEmpty { part("⟳ \(item.claimedBy)", item.claimWorking ? Palette.positive : Palette.gray) }
        if !item.prs.isEmpty { part("PR", Palette.accent) }
        for label in item.labels.prefix(4) { part("#\(label)", Palette.purple) }
        var out = AttributedString()
        for (index, piece) in parts.enumerated() {
            if index > 0 {
                var dot = AttributedString(" · ")
                dot.foregroundColor = Palette.muted
                out += dot
            }
            out += piece
        }
        return out
    }

    private var accessibility: String {
        var parts = ["\(item.id), \(item.title)", item.kind.label, item.statusLabel]
        if item.priority != .noPriority { parts.append("\(item.priority.label) priority") }
        if item.blocker { parts.append("blocker") }
        if item.blocked { parts.append("blocked") }
        if !needs.isEmpty { parts.append("needs you: \(needs.map(\.label).joined(separator: ", "))") }
        if !item.claimedBy.isEmpty { parts.append("claimed by \(item.claimedBy)") }
        return parts.joined(separator: ", ")
    }
}
