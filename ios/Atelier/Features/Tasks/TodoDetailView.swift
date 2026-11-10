import SwiftUI

/// Everything about one todo: its fields, the proposal, the claim, the
/// checklist (tap to tick; add, reword, remove), the body as a page, links,
/// relations and the log; the changes the desktop offers you for it.
/// Viewing never starts or resumes a session.
struct TodoDetailView: View {
    @ObservedObject var connection: ServiceConnection
    let id: String
    var push: (Route) -> Void
    @Environment(\.openURL) private var openURL
    @State private var editing = false
    @State private var declining = false
    @State private var canceling = false
    @State private var reason = ""
    @State private var newCheck = ""
    @State private var rewording: Int?
    @State private var rewordText = ""
    @State private var link: URL?
    @State private var showAllLog = false
    @State private var body_: ReportWebController?
    @State private var bodyHeight: CGFloat = 80

    var body: some View {
        // Read again whenever the boards change (the service's book, your changes).
        let detail = connection.todos.flatMap { _ in connection.client.todo(id: id) }
        Group {
            if let detail {
                content(detail)
            } else if connection.todos?.loaded == true {
                ContentUnavailableView("No todo \(id)", systemImage: "checklist", description: Text("It is not on this canvas's boards (deleted, or a mistyped id)."))
            } else {
                ProgressView("Loading the boards…")
            }
        }
        .background(Palette.background)
        .navigationTitle(detail?.item.id ?? id)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if let detail {
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button("Edit") { editing = true }
                        .disabled(connection.state != .online)
                        .accessibilityIdentifier("editTodo")
                    Menu { TodoMenu(connection: connection, item: detail.item, decline: { declining = true }) } label: { Image(systemName: "ellipsis.circle") }
                        .accessibilityLabel("Todo actions")
                        .accessibilityIdentifier("todoActions")
                }
            }
        }
        .sheet(isPresented: $editing) {
            if let detail { TodoEditView(connection: connection, detail: detail) }
        }
        .alert("Decline \(id)?", isPresented: $declining) {
            TextField("Why (optional)", text: $reason)
            Button("Decline", role: .destructive) {
                connection.todoChange { try $0.declineTodo(id: id, reason: reason) }
                reason = ""
            }
            Button("Cancel", role: .cancel) { reason = "" }
        }
        .alert("Cancel \(id)?", isPresented: $canceling) {
            TextField("Why (optional)", text: $reason)
            Button("Cancel the todo", role: .destructive) {
                connection.todoChange { try $0.cancelTodo(id: id, reason: reason) }
                reason = ""
            }
            Button("Keep it", role: .cancel) { reason = "" }
        } message: {
            Text("It moves to Canceled (won't do). Reopen brings it back.")
        }
        .alert("Checklist item", isPresented: Binding(get: { rewording != nil }, set: { if !$0 { rewording = nil } })) {
            TextField("Item", text: $rewordText)
            Button("Save") {
                if let index = rewording { connection.todoChange(say: false) { try $0.editCheck(id: id, index: UInt32(index), text: rewordText) } }
            }
            Button("Cancel", role: .cancel) {}
        }
        .confirmationDialog("Open this link?", isPresented: Binding(get: { link != nil }, set: { if !$0 { link = nil } }), titleVisibility: .visible, presenting: link) { url in
            Button("Open in Safari") { openURL(url) }
        } message: { url in
            Text(url.absoluteString)
        }
    }

    private func content(_ detail: TodoDetail) -> some View {
        let item = detail.item
        return List {
            SwiftUI.Section {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        Text(item.kindGlyph).font(.body.monospaced().weight(.bold)).foregroundStyle(item.kind.color)
                        Text(item.kind.label).font(.caption.weight(.semibold)).foregroundStyle(item.kind.color)
                        if !item.priorityMarks.isEmpty {
                            Text(item.priorityMarks).font(.caption.weight(.heavy)).foregroundStyle(item.priority == .urgent ? Palette.failed : Palette.attention)
                        }
                        if item.blocker { Text("‼\u{FE0E} blocker").font(.caption.weight(.bold)).foregroundStyle(Palette.failed) }
                        Spacer()
                        Text(item.statusLabel)
                            .font(.caption.weight(.semibold))
                            .padding(.horizontal, 8)
                            .padding(.vertical, 2)
                            .background(item.status.color.opacity(0.18), in: Capsule())
                            .foregroundStyle(item.status.color)
                            .accessibilityIdentifier("todoStatus")
                    }
                    Text(item.title)
                        .font(.title3.weight(.semibold))
                        .foregroundStyle(Palette.text)
                        .accessibilityIdentifier("todoTitle")
                    if item.blocked {
                        Label(item.blockedReason.isEmpty ? "Blocked" : item.blockedReason, systemImage: "flag.fill")
                            .font(.footnote)
                            .foregroundStyle(Palette.failed)
                    }
                    if !item.labels.isEmpty {
                        Text(item.labels.map { "#\($0)" }.joined(separator: "  ")).font(.caption).foregroundStyle(Palette.purple)
                    }
                }
                .padding(.vertical, 4)
                field("Board", item.lane.isEmpty ? item.boardName : "\(rootName(item)) › \(item.lane)")
                field("Priority", item.priority.label)
                if !detail.createdBy.isEmpty { field("Created by", detail.createdBy + (item.aiCreated ? " ◇" : "")) }
                if !detail.resolution.isEmpty { field("Resolution", detail.resolution.replacingOccurrences(of: "_", with: " ")) }
            }
            .listRowBackground(Palette.panel)

            if !item.proposalWhy.isEmpty || detail.actions.accept {
                SwiftUI.Section("Proposed · why") {
                    Text(item.proposalWhy.isEmpty ? "(no reason given)" : item.proposalWhy).font(.callout).foregroundStyle(Palette.purple)
                    if detail.actions.accept {
                        Menu {
                            ForEach([TodoStatus.backlog, .todo, .inProgress], id: \.self) { status in
                                Button(status.label) { connection.todoChange { try $0.acceptTodo(id: id, status: status) } }
                            }
                        } label: {
                            Label("Accept into…", systemImage: "checkmark.circle.fill")
                        } primaryAction: {
                            connection.todoChange { try $0.acceptTodo(id: id, status: nil) }
                        }
                        .accessibilityIdentifier("acceptTodo")
                    }
                    if detail.actions.decline {
                        Button(role: .destructive) { declining = true } label: { Label("Decline…", systemImage: "xmark.circle") }
                            .accessibilityIdentifier("declineTodo")
                    }
                }
                .listRowBackground(Palette.panel)
            }

            actions(detail)

            checklist(detail)

            if !detail.bodyHtml.isEmpty {
                SwiftUI.Section("Description") {
                    TodoBodyView(controller: body_)
                        .frame(height: bodyHeight)
                        .listRowInsets(EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 0))
                }
                .listRowBackground(Color(rgb: TerminalColors.background))
            }

            if !detail.links.isEmpty {
                SwiftUI.Section("Links") {
                    ForEach(detail.links, id: \.target) { link in linkRow(link) }
                }
                .listRowBackground(Palette.panel)
            }

            if !detail.relations.isEmpty {
                SwiftUI.Section("Related") {
                    ForEach(detail.relations, id: \.id) { relation in
                        NavigationLink(value: Route.todo(relation.id)) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text("\(relation.kind) \(relation.id)").font(.caption).foregroundStyle(Palette.muted)
                                Text(relation.title.isEmpty ? "(not on this canvas)" : relation.title)
                                    .foregroundStyle(relation.closed ? Palette.gray : Palette.text)
                                    .strikethrough(relation.closed)
                            }
                        }
                    }
                }
                .listRowBackground(Palette.panel)
            }

            if !detail.log.isEmpty {
                SwiftUI.Section("Log") {
                    ForEach(Array((showAllLog ? detail.log : Array(detail.log.prefix(6))).enumerated()), id: \.offset) { _, line in
                        VStack(alignment: .leading, spacing: 2) {
                            Text("\(line.by) · \(Ago.short(line.ms)) ago").font(.caption2).foregroundStyle(Palette.muted)
                            Text(line.text).font(line.note ? .callout : .caption).foregroundStyle(line.note ? Palette.text : Palette.gray)
                        }
                    }
                    if detail.log.count > 6 {
                        Button(showAllLog ? "Show less" : "Show all \(detail.log.count)") { showAllLog.toggle() }.font(.caption)
                    }
                }
                .listRowBackground(Palette.panel)
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .scrollDismissesKeyboard(.interactively)
        .task(id: detail.bodyHtml) { await loadBody(detail.bodyHtml) }
    }

    private func rootName(_ item: TodoItem) -> String {
        connection.todos?.boards.first { $0.id == item.rootBoard }?.name ?? item.boardName
    }

    private func field(_ name: String, _ value: String) -> some View {
        LabeledContent(name) {
            Text(value).foregroundStyle(Palette.text).multilineTextAlignment(.trailing)
        }
        .font(.callout)
    }

    /// Move, Done, Cancel, Reopen, Release: only those the desktop offers now.
    @ViewBuilder
    private func actions(_ detail: TodoDetail) -> some View {
        let a = detail.actions
        if a.done || a.cancel || a.reopen || a.release || !a.moves.isEmpty || !detail.item.claimedBy.isEmpty {
            SwiftUI.Section {
                if !detail.item.claimedBy.isEmpty {
                    HStack {
                        Label("Claimed by \(detail.item.claimedBy)", systemImage: detail.item.claimWorking ? "circle.dotted" : "person.crop.circle")
                            .foregroundStyle(detail.item.claimWorking ? Palette.positive : Palette.text)
                        Spacer()
                        if !detail.item.claimedSession.isEmpty, connection.overview?.sessions.contains(where: { $0.id == detail.item.claimedSession && $0.alive }) == true {
                            Button("View") { push(.terminal(detail.item.claimedSession)) }.buttonStyle(.borderless)
                        }
                    }
                    .font(.callout)
                }
                if !a.moves.isEmpty {
                    Menu {
                        ForEach(a.moves, id: \.self) { status in
                            Button(status.label) { connection.todoChange { try $0.moveTodo(id: id, status: status) } }
                        }
                    } label: {
                        Label("Move to…", systemImage: "arrow.right.circle")
                    }
                    .accessibilityIdentifier("moveTodo")
                }
                if a.done {
                    Button { connection.todoChange { try $0.doneTodo(id: id) } } label: { Label("Done", systemImage: "checkmark.circle") }
                        .accessibilityIdentifier("doneTodo")
                }
                if a.release {
                    Button { connection.todoChange { try $0.releaseTodo(id: id) } } label: { Label("Release the claim", systemImage: "person.crop.circle.badge.xmark") }
                }
                if a.reopen {
                    Button { connection.todoChange { try $0.reopenTodo(id: id) } } label: { Label("Reopen", systemImage: "arrow.uturn.backward.circle") }
                        .accessibilityIdentifier("reopenTodo")
                }
                if a.cancel && !a.decline {
                    Button(role: .destructive) { canceling = true } label: { Label("Cancel the todo…", systemImage: "xmark.circle") }
                        .accessibilityIdentifier("cancelTodo")
                }
            }
            .listRowBackground(Palette.panel)
        }
    }

    private func checklist(_ detail: TodoDetail) -> some View {
        SwiftUI.Section {
            ForEach(Array(detail.checklist.enumerated()), id: \.offset) { index, check in
                Button {
                    connection.todoChange(say: false) { try $0.checkTodo(id: id, index: UInt32(index), done: !check.done) }
                } label: {
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: check.done ? "checkmark.square.fill" : "square")
                            .foregroundStyle(check.done ? Palette.loop : Palette.gray)
                        Text(check.text)
                            .foregroundStyle(check.done ? Palette.gray : Palette.text)
                            .strikethrough(check.done)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    // The whole row ticks, not only its words.
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("checkItem")
                .accessibilityLabel(check.text)
                .accessibilityValue(check.done ? "done" : "not done")
                .accessibilityAddTraits(check.done ? .isSelected : [])
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) { connection.todoChange(say: false) { try $0.removeCheck(id: id, index: UInt32(index)) } } label: { Label("Remove", systemImage: "trash") }
                    Button {
                        rewordText = check.text
                        rewording = index
                    } label: { Label("Edit", systemImage: "pencil") }
                        .tint(Palette.finished)
                }
                .contextMenu {
                    Button {
                        rewordText = check.text
                        rewording = index
                    } label: { Label("Edit", systemImage: "pencil") }
                    Button(role: .destructive) { connection.todoChange(say: false) { try $0.removeCheck(id: id, index: UInt32(index)) } } label: { Label("Remove", systemImage: "trash") }
                }
            }
            HStack {
                Image(systemName: "plus").foregroundStyle(Palette.accent)
                TextField("Add an item", text: $newCheck)
                    .submitLabel(.done)
                    .onSubmit(addCheck)
                    .accessibilityIdentifier("addCheckItem")
            }
        } header: {
            let done = detail.checklist.filter(\.done).count
            Text(detail.checklist.isEmpty ? "Checklist" : "Checklist \(done)/\(detail.checklist.count)")
        }
        .listRowBackground(Palette.panel)
    }

    private func addCheck() {
        let text = newCheck.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        if connection.todoChange(say: false, { try $0.addCheck(id: id, text: text) }) != nil {
            newCheck = ""
        }
    }

    @ViewBuilder
    private func linkRow(_ link: TodoLink) -> some View {
        let title = link.title.isEmpty ? link.target : link.title
        switch link.kind {
        case "note", "file":
            Button { push(.note(link.target)) } label: { Label(title, systemImage: "doc.text") }
        case "session":
            let session = connection.overview?.sessions.first { $0.id == link.target }
            if let session, session.alive {
                Button { push(.terminal(session.id)) } label: { Label("View “\(session.name)”", systemImage: "terminal") }
            } else {
                Label("\(session?.name ?? link.target) (not running)", systemImage: "terminal").foregroundStyle(Palette.muted)
            }
        default:
            Button {
                if reportLinkAllowed(url: link.target), let url = URL(string: link.target) { self.link = url }
            } label: {
                Label(title, systemImage: link.kind == "pr" ? "arrow.triangle.pull" : "link")
            }
        }
    }

    private func loadBody(_ html: String) async {
        guard !html.isEmpty, let rules = try? await ReportLockdown.ruleList() else { return }
        let controller = body_ ?? ReportWebController(rules: rules, identifier: "todoBody")
        controller.webView.scrollView.isScrollEnabled = false
        controller.onLink = { url in link = url }
        controller.onAppLink = { target in
            switch target {
            case .todo(let other): push(.todo(other))
            case .note(let name):
                Task {
                    if let found = try? await connection.perform({ try $0.resolveNote(name: name) }) { push(.note(found)) } else { connection.toast = Toast(text: "No note named “\(name)”.", error: true) }
                }
            }
        }
        controller.onFinish = { [weak controller] in
            Task { @MainActor in
                if let height = await controller?.contentHeight(), height > 0 { bodyHeight = min(max(height, 44), 4000) }
            }
        }
        controller.load(html)
        body_ = controller
    }
}

/// A todo's body in the locked-down web view (sized to its page).
struct TodoBodyView: View {
    let controller: ReportWebController?

    var body: some View {
        if let controller {
            ReportWebView(controller: controller).id(ObjectIdentifier(controller))
        } else {
            ProgressView()
        }
    }
}
