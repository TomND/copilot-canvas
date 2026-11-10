import SwiftUI

/// A new todo: title, board and swimlane, type, priority, status, blocker,
/// body. Added as you (the service confirms it).
struct NewTodoView: View {
    @ObservedObject var connection: ServiceConnection
    var created: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @AppStorage("todoBoard") private var selectedBoard = ""
    @State private var title = ""
    @State private var board = ""
    @State private var kind: TodoKind = .feature
    @State private var priority: TodoPriority = .medium
    @State private var status: TodoStatus = .backlog
    @State private var blocker = false
    @State private var bodyText = ""
    @FocusState private var titleFocused: Bool

    /// Boards and their swimlanes: (lane id, "Board › Lane").
    private var places: [(id: String, name: String)] {
        (connection.todos?.boards ?? []).flatMap { board in
            board.lanes.enumerated().map { index, lane in (lane.id, index == 0 ? board.name : "\(board.name) › \(lane.name)") }
        }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Title", text: $title, axis: .vertical)
                        .focused($titleFocused)
                        .accessibilityIdentifier("todoTitleField")
                    Picker("Board", selection: $board) {
                        ForEach(places, id: \.id) { Text($0.name).tag($0.id) }
                    }
                    .accessibilityIdentifier("todoBoardField")
                }
                Section {
                    Picker("Type", selection: $kind) {
                        ForEach(TodoKind.all, id: \.self) { Text($0.label).tag($0) }
                    }
                    Picker("Priority", selection: $priority) {
                        ForEach(TodoPriority.all, id: \.self) { Text($0.label).tag($0) }
                    }
                    Picker("Status", selection: $status) {
                        ForEach([TodoStatus.backlog, .todo, .inProgress], id: \.self) { Text($0.label).tag($0) }
                    }
                    Toggle("Blocks other work (‼\u{FE0E})", isOn: $blocker)
                }
                Section {
                    TextEditor(text: $bodyText)
                        .font(.system(.body, design: .monospaced))
                        .autocorrectionDisabled()
                        .frame(minHeight: 120)
                        .accessibilityIdentifier("todoBodyField")
                } header: {
                    Text("Description (Markdown, optional)")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.background)
            .scrollDismissesKeyboard(.interactively)
            .keyboardDoneButton()
            .navigationTitle("New todo")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add") { add() }
                        .fontWeight(.semibold)
                        .disabled(title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || board.isEmpty)
                        .accessibilityIdentifier("addTodo")
                }
            }
            .onAppear {
                if board.isEmpty { board = places.first { $0.id == selectedBoard }?.id ?? places.first?.id ?? "" }
                titleFocused = true
            }
        }
    }

    private func add() {
        let draft = TodoDraft(board: board, title: title.trimmingCharacters(in: .whitespacesAndNewlines), kind: kind, priority: priority, status: status, body: bodyText, blocker: blocker)
        if let change = connection.todoChange({ try $0.addTodo(draft: draft) }) {
            created(change.id)
        }
    }
}

/// A todo's fields: title, type, priority, labels, blocker and blocked
/// reason, body. Only what you changed is sent.
struct TodoEditView: View {
    @ObservedObject var connection: ServiceConnection
    let detail: TodoDetail
    @Environment(\.dismiss) private var dismiss
    @State private var title: String
    @State private var kind: TodoKind
    @State private var priority: TodoPriority
    @State private var labels: String
    @State private var blocker: Bool
    @State private var blocked: String
    @State private var bodyText: String

    init(connection: ServiceConnection, detail: TodoDetail) {
        self.connection = connection
        self.detail = detail
        _title = State(initialValue: detail.item.title)
        _kind = State(initialValue: detail.item.kind)
        _priority = State(initialValue: detail.item.priority)
        _labels = State(initialValue: detail.item.labels.joined(separator: ", "))
        _blocker = State(initialValue: detail.item.blocker)
        _blocked = State(initialValue: detail.blockedText)
        _bodyText = State(initialValue: detail.body)
    }

    private var labelList: [String] {
        labels.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }

    private var edit: TodoEdit {
        let item = detail.item
        let newTitle = title.trimmingCharacters(in: .whitespacesAndNewlines)
        return TodoEdit(
            title: newTitle != item.title ? newTitle : nil,
            kind: kind != item.kind ? kind : nil,
            priority: priority != item.priority ? priority : nil,
            body: bodyText != detail.body ? bodyText : nil,
            labels: labelList != item.labels ? labelList : nil,
            blocker: blocker != item.blocker ? blocker : nil,
            blocked: blocked.trimmingCharacters(in: .whitespaces) != detail.blockedText ? blocked : nil
        )
    }

    private var changed: Bool { edit != TodoEdit(title: nil, kind: nil, priority: nil, body: nil, labels: nil, blocker: nil, blocked: nil) }

    var body: some View {
        NavigationStack {
            Form {
                Section("Title") {
                    TextField("Title", text: $title, axis: .vertical)
                        .accessibilityIdentifier("todoTitleField")
                }
                Section {
                    Picker("Type", selection: $kind) {
                        ForEach(TodoKind.all, id: \.self) { Text($0.label).tag($0) }
                    }
                    Picker("Priority", selection: $priority) {
                        ForEach(TodoPriority.all, id: \.self) { Text($0.label).tag($0) }
                    }
                    TextField("Labels, comma separated", text: $labels)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("todoLabelsField")
                }
                Section {
                    Toggle("Blocks other work (‼\u{FE0E})", isOn: $blocker)
                    TextField("Blocked because… (empty: not blocked)", text: $blocked, axis: .vertical)
                        .accessibilityIdentifier("todoBlockedField")
                } footer: {
                    Text("Todos it waits for (blocked-by relations) are kept on the desktop.")
                }
                Section("Description (Markdown)") {
                    TextEditor(text: $bodyText)
                        .font(.system(.body, design: .monospaced))
                        .autocorrectionDisabled()
                        .frame(minHeight: 160)
                        .accessibilityIdentifier("todoBodyField")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.background)
            .scrollDismissesKeyboard(.interactively)
            .keyboardDoneButton()
            .navigationTitle("Edit \(detail.item.id)")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { save() }
                        .fontWeight(.semibold)
                        .disabled(!changed || title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        .accessibilityIdentifier("saveTodo")
                }
            }
        }
        .interactiveDismissDisabled(changed)
    }

    private func save() {
        let id = detail.item.id, edit = edit
        if connection.todoChange({ try $0.updateTodo(id: id, edit: edit) }) != nil {
            dismiss()
        }
    }
}
