import SwiftUI

/// A new session on the service's machine: name, agent, folder, group, first prompt.
struct NewSessionView: View {
    @ObservedObject var connection: ServiceConnection
    /// A subsession of this session (linked to it; may fork its conversation).
    var parent: SessionItem? = nil
    var started: (String) -> Void

    @Environment(\.dismiss) private var dismiss
    @AppStorage("terminalFontSize") private var fontSize: Double = 12.5
    @State private var name = ""
    @State private var agent = ""
    @State private var folder = ""
    @State private var group = ""
    @State private var prompt = ""
    @State private var busy = false
    @State private var error: String?
    @State private var browsing = false
    @State private var fork = false
    @State private var canFork = false
    @FocusState private var focus: Field?

    private enum Field: Hashable {
        case name, folder, prompt
    }

    private var overview: Overview? { connection.overview }

    var body: some View {
        NavigationStack {
            ScrollViewReader { proxy in
                Form {
                    if let parent {
                        Section {
                            Label(parent.name, systemImage: "arrow.turn.down.right")
                            if canFork {
                                Toggle("Fork its conversation", isOn: $fork)
                                    .accessibilityIdentifier("fork")
                            }
                        } header: {
                            Text("Subsession of")
                        } footer: {
                            Text(canFork ? "A fork starts from a copy of the conversation (history and checkpoints); the parent is untouched. Off: a fresh conversation." : "This session's conversation cannot be forked here: the subsession starts fresh.")
                        }
                    }
                    Section("Session") {
                        TextField("Name (default: the folder's name)", text: $name)
                            .focused($focus, equals: .name)
                            .submitLabel(.next)
                            .onSubmit { focus = .folder }
                        if let agents = overview?.agents, !agents.isEmpty, !(fork && canFork) {
                            Picker("Agent", selection: $agent) {
                                ForEach(agents, id: \.self) { Text(label($0)).tag($0) }
                            }
                        }
                        if let groups = overview?.groups, !groups.isEmpty {
                            Picker("Group", selection: $group) {
                                Text("None").tag("")
                                ForEach(groups, id: \.id) { Text($0.name).tag($0.id) }
                            }
                        }
                    }
                    Section {
                        TextField("Folder on the Cloud PC", text: $folder, axis: .vertical)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .font(.system(.body, design: .monospaced))
                            .focused($focus, equals: .folder)
                            .submitLabel(.next)
                            .onChange(of: folder) { _, text in
                                // Return in this (wrapping) field moves on to the prompt.
                                if text.contains("\n") {
                                    folder = text.replacingOccurrences(of: "\n", with: "")
                                    focus = .prompt
                                }
                            }
                        // A button, not a link: the keyboard closes and stays closed after picking.
                        Button {
                            focus = nil
                            Keyboard.dismiss()
                            browsing = true
                        } label: {
                            HStack {
                                Text("Browse…")
                                Spacer()
                                Image(systemName: "chevron.right").font(.footnote.weight(.semibold)).foregroundStyle(.tertiary)
                            }
                        }
                        .accessibilityIdentifier("browse")
                        if let recent = overview?.recentFolders, !recent.isEmpty {
                            Menu("Recent folders") {
                                ForEach(recent.prefix(12), id: \.self) { path in
                                    Button(path) {
                                        focus = nil
                                        folder = path
                                    }
                                }
                            }
                        }
                    } header: {
                        Text("Folder")
                    }
                    Section {
                        TextEditor(text: $prompt)
                            .frame(minHeight: 90)
                            .focused($focus, equals: .prompt)
                            .accessibilityIdentifier("prompt")
                            .id("prompt")
                    } header: {
                        Text("First prompt (optional)")
                    }
                    if let error {
                        Section { Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(Palette.failed) }
                    }
                }
                .scrollContentBackground(.hidden)
                .scrollDismissesKeyboard(.interactively)
                .background(Palette.background)
                .onChange(of: focus) { _, field in
                    // The prompt sits above the keyboard.
                    guard field == .prompt else { return }
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) {
                        withAnimation { proxy.scrollTo("prompt", anchor: .bottom) }
                    }
                }
            }
            .keyboardDoneButton()
            .navigationDestination(isPresented: $browsing) {
                FolderBrowser(connection: connection, start: folder.isEmpty ? (overview?.homeFolder ?? "") : folder) { picked in
                    focus = nil
                    folder = picked
                }
            }
            .navigationTitle(parent == nil ? "New session" : "New subsession")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        Keyboard.dismiss()
                        dismiss()
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if busy { ProgressView() } else { Button("Start") { start() }.disabled(folder.trimmingCharacters(in: .whitespaces).isEmpty) }
                }
            }
            .onAppear {
                if let parent {
                    agent = parent.agent
                    if folder.isEmpty { folder = parent.cwd }
                }
                if agent.isEmpty { agent = overview?.agents.first ?? "" }
                if folder.isEmpty { folder = overview?.recentFolders.first ?? "" }
            }
            .task {
                // Copilot forks need the parent's history on the service's machine.
                guard let parent, parent.forkable else { return }
                let id = parent.id
                let possible = (try? await connection.perform { $0.canFork(id: id) }) ?? false
                canFork = possible
                fork = possible
            }
        }
    }

    private func label(_ agent: String) -> String {
        switch agent {
        case "opencode": return "OpenCode"
        case "claude": return "Claude Code"
        default: return "GitHub Copilot"
        }
    }

    private func start() {
        focus = nil
        Keyboard.dismiss()
        busy = true
        error = nil
        let request = NewSession(name: name, agent: agent, cwd: folder, prompt: prompt, group: group, parent: parent?.id ?? "", fork: fork && canFork)
        // About the size of this phone's terminal; it follows the screen once open.
        let fonts = Fonts(size: CGFloat(fontSize))
        let bounds = UIScreen.main.bounds
        let cols = UInt16(max(20, Int((bounds.width - 8) / fonts.cellWidth)))
        let rows = UInt16(max(10, Int((bounds.height - 200) / fonts.cellHeight)))
        Task {
            do {
                let id = try await connection.perform { try $0.newSession(request: request, cols: cols, rows: rows) }
                busy = false
                started(id)
            } catch {
                busy = false
                self.error = describe(error)
            }
        }
    }
}

/// Folders of the service's machine (read through the service).
struct FolderBrowser: View {
    @ObservedObject var connection: ServiceConnection
    let start: String
    var picked: (String) -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var current = ""
    @State private var entries: [String] = []
    @State private var loading = false
    @State private var error: String?

    var body: some View {
        List {
            if let error { Text(error).foregroundStyle(Palette.failed) }
            ForEach(entries, id: \.self) { entry in
                Button {
                    load(entry == ".." ? parent(of: current) : join(current, entry))
                } label: {
                    Label(entry, systemImage: entry == ".." ? "arrow.turn.left.up" : "folder")
                }
            }
        }
        .overlay { if loading { ProgressView() } }
        .navigationTitle(current.isEmpty ? "Drives" : (current as NSString).lastPathComponent)
        .navigationBarTitleDisplayMode(.inline)
        .safeAreaInset(edge: .bottom) {
            Button {
                picked(current)
                dismiss()
            } label: {
                Text("Use \(current.isEmpty ? "this folder" : current)").lineLimit(1).frame(maxWidth: .infinity)
            }
            .accessibilityIdentifier("useFolder")
            .buttonStyle(.borderedProminent)
            .disabled(current.isEmpty)
            .padding()
            .background(.bar)
        }
        .onAppear { load(start) }
    }

    /// Windows paths (`C:\…`) on the Cloud PC, POSIX paths on the mock.
    private var separator: String { current.contains("\\") || current.hasSuffix(":") ? "\\" : "/" }

    private func join(_ base: String, _ name: String) -> String {
        if base.isEmpty { return name }
        return base.hasSuffix(separator) ? base + name : base + separator + name
    }

    private func parent(of path: String) -> String {
        let trimmed = path.hasSuffix(separator) && path.count > 3 ? String(path.dropLast()) : path
        guard let index = trimmed.range(of: separator, options: .backwards) else { return "" }
        let up = String(trimmed[..<index.lowerBound])
        if up.isEmpty { return separator == "/" ? "/" : "" }
        return up.hasSuffix(":") ? up + "\\" : up
    }

    private func load(_ folder: String) {
        loading = true
        error = nil
        Task {
            do {
                let listing = try await connection.perform { try $0.listFolder(folder: folder.isEmpty ? nil : folder) }
                current = folder
                entries = listing.entries
            } catch {
                self.error = describe(error)
            }
            loading = false
        }
    }
}
