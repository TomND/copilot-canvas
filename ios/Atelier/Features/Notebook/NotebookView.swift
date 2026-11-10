import SwiftUI

extension NoteEntry: Identifiable {
    public var id: String { path }
}

extension NoteHit: Identifiable {
    public var id: String { "\(path)#\(line)" }
}

extension NotebookRoot: Identifiable {
    public var id: String { path }
}

extension NoteFilter {
    /// The filter as it is kept between launches.
    init(stored: String) {
        switch stored {
        case "human": self = .human
        case "ai": self = .ai
        default: self = .all
        }
    }

    var stored: String {
        switch self {
        case .all: return "all"
        case .human: return "human"
        case .ai: return "ai"
        }
    }
}

extension NoteProvenance {
    var forYou: Bool { audience == .human }
    /// `●` for you, `◇` for agents (the desktop's marks).
    var marker: String { forYou ? "●" : "◇" }
    var audienceText: String { forYou ? "For you" : "For agents" }

    var agentName: String {
        switch agent {
        case "copilot": return "Copilot"
        case "opencode": return "OpenCode"
        case "claude": return "Claude Code"
        default: return "an agent"
        }
    }

    /// "written by you", "written by OpenCode", "written by Copilot and you".
    var writer: String {
        switch author {
        case .human: return "written by you"
        case .ai: return "written by \(agentName)"
        case .mixed: return "written by \(agentName) and you"
        }
    }

    /// One line: "For you · written by Copilot · Fix flaky test · reviewed".
    func summary(inferredMark: Bool = false) -> String {
        var parts = ["\(audienceText)\(inferredMark && inferred ? " (inferred)" : "")", writer]
        if !sessionName.isEmpty { parts.append(sessionName) }
        if reviewed { parts.append("reviewed") }
        if passive { parts.append("passive memory") }
        return parts.joined(separator: " · ")
    }
}

extension NoteKind {
    var symbol: String {
        switch self {
        case .folder: return "folder.fill"
        case .markdown: return "doc.text"
        case .html: return "doc.richtext"
        case .text: return "doc.plaintext"
        case .image: return "photo"
        case .other: return "doc"
        }
    }

    /// The phone edits the source of these.
    var editable: Bool { self == .markdown || self == .html || self == .text }
    /// These carry a reviewed mark.
    var reviewable: Bool { self == .markdown || self == .html }
}

/// The Notebook place: the notebook root's folder, the other roots (group
/// folders outside it, the canvas-wide folder, mounts) and search.
struct NotebookHome: View {
    @ObservedObject var connection: ServiceConnection
    @Binding var create: NoteCreate?
    var push: (Route) -> Void
    @State private var overview: NotebookOverview?
    @State private var error: String?

    var body: some View {
        Group {
            if let overview {
                NoteFolderView(connection: connection, folder: overview.root, title: "Notebook", home: true, roots: overview.roots, create: $create, push: push)
            } else if let error {
                ContentUnavailableView {
                    Label("The notebook cannot be shown", systemImage: "book.closed")
                } description: {
                    Text(error)
                } actions: {
                    Button("Try again") { Task { await load() } }
                }
            } else if connection.state == .online {
                VStack(spacing: 12) {
                    ProgressView()
                    Text("Opening the notebook…").foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView("Offline", systemImage: "wifi.exclamationmark", description: Text("The notebook shows once \(connection.server.name) is connected."))
            }
        }
        .task(id: connection.state) {
            if connection.state == .online && overview == nil { await load() }
        }
    }

    private func load() async {
        error = nil
        do {
            overview = try await connection.perform { try $0.notebook() }
        } catch {
            // An older service does not know the notebook's requests.
            self.error = describe(error)
        }
    }
}

/// A notebook folder: its folders and notes with their provenance (notes for
/// agents dimmed), the For you / For agents filter, the canvas's own files
/// on request; new notes and folders, rename, delete to the trash.
struct NoteFolderView: View {
    @ObservedObject var connection: ServiceConnection
    let folder: String
    let title: String
    /// The notebook's own root (the place's first screen: search, the other roots).
    var home = false
    var roots: [NotebookRoot] = []
    /// What the + asked for (the place's toolbar on the first screen).
    var create: Binding<NoteCreate?>? = nil
    var push: (Route) -> Void
    @AppStorage("noteFilter") private var storedFilter = "all"
    @AppStorage("noteShowInternal") private var showInternal = false
    @State private var listing: NoteListing?
    @State private var error: String?
    @State private var localCreate: NoteCreate?
    @State private var newName = ""
    @State private var renaming: NoteEntry?
    @State private var deleting: NoteEntry?
    @State private var query = ""
    @State private var hits: [NoteHit] = []
    @State private var searching = false

    private var filter: Binding<NoteFilter> {
        Binding(get: { NoteFilter(stored: storedFilter) }, set: { storedFilter = $0.stored })
    }

    private var creating: Binding<NoteCreate?> { create ?? $localCreate }

    var body: some View {
        list
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .task(id: "\(folder)|\(storedFilter)|\(showInternal)|\(connection.state == .online)") { await load() }
            // Back from a note (renamed, deleted, new): the folder again.
            .onAppear { if listing != nil { Task { await load() } } }
            .toolbar {
                if !home {
                    ToolbarItem(placement: .topBarTrailing) {
                        Menu {
                            Button { localCreate = .note } label: { Label("New note", systemImage: "doc.badge.plus") }
                            Button { localCreate = .folder } label: { Label("New folder", systemImage: "folder.badge.plus") }
                        } label: { Image(systemName: "plus") }
                            .accessibilityLabel("New note or folder")
                            .accessibilityIdentifier("newNoteMenu")
                    }
                }
            }
            .alert(creating.wrappedValue == .folder ? "New folder" : "New note", isPresented: Binding(get: { creating.wrappedValue != nil }, set: { if !$0 { creating.wrappedValue = nil } })) {
                TextField(creating.wrappedValue == .folder ? "Folder name" : "Note name", text: $newName)
                    .accessibilityIdentifier("newNoteName")
                Button("Create") { make(creating.wrappedValue ?? .note) }
                Button("Cancel", role: .cancel) { newName = "" }
            } message: {
                Text(creating.wrappedValue == .folder ? "In \(title)." : "In \(title). Markdown unless you type .html or .txt; written as a note for you.")
            }
            .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
                TextField("Name", text: $newName)
                Button("Rename") { rename() }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("A note keeps its extension when you type none.")
            }
            .confirmationDialog("Delete “\(deleting?.name ?? "")”?", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }), titleVisibility: .visible, presenting: deleting) { entry in
                Button("Move to the trash", role: .destructive) { trash(entry) }
            } message: { _ in
                Text("It moves to the notebook's .trash folder on \(connection.server.name), where it can be taken back.")
            }
    }

    @ViewBuilder
    private var list: some View {
        let content = List {
            if home && !query.trimmingCharacters(in: .whitespaces).isEmpty {
                results
            } else {
                if home && roots.count > 1 {
                    Section("Other folders") {
                        ForEach(roots.dropFirst()) { root in
                            NavigationLink(value: Route.folder(root.path, root.label)) {
                                Label(root.label, systemImage: root.kind == .canvasWide ? "sparkles" : root.kind == .group ? "rectangle.3.group" : "externaldrive")
                            }
                            .listRowBackground(Palette.panel)
                        }
                    }
                }
                Section {
                    filterRow
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
                }
                entries
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .scrollDismissesKeyboard(.immediately)
        .refreshable { await load() }
        if home {
            content
                .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .automatic), prompt: "Search notes")
                .task(id: query) { await search() }
        } else {
            content
        }
    }

    private var filterRow: some View {
        HStack(spacing: 8) {
            Picker("Show", selection: filter) {
                Text("All").tag(NoteFilter.all)
                Text("For you").tag(NoteFilter.human)
                Text("For agents").tag(NoteFilter.ai)
            }
            .pickerStyle(.segmented)
            .accessibilityIdentifier("noteFilter")
            Menu {
                Toggle(isOn: $showInternal) { Label("Show the canvas's own files", systemImage: "gearshape.2") }
                Button { UIPasteboard.general.string = folder } label: { Label("Copy folder path", systemImage: "doc.on.doc") }
            } label: {
                Image(systemName: showInternal ? "line.3.horizontal.decrease.circle.fill" : "line.3.horizontal.decrease.circle")
                    .font(.title3)
            }
            .accessibilityLabel("Folder options")
            .accessibilityIdentifier("noteOptions")
        }
    }

    @ViewBuilder
    private var entries: some View {
        if let listing {
            Section {
                if listing.entries.isEmpty {
                    Text(listing.hidden > 0 ? "Nothing for this filter." : "This folder is empty.")
                        .font(.callout)
                        .foregroundStyle(Palette.muted)
                        .listRowBackground(Palette.panel)
                }
                ForEach(listing.entries) { entry in
                    row(entry)
                }
            } footer: {
                if let footer = footer(listing) { Text(footer) }
            }
        } else if let error {
            Section {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(Palette.failed)
                    .listRowBackground(Palette.panel)
            }
        } else {
            Section {
                HStack {
                    ProgressView()
                    Text("Loading the folder…").foregroundStyle(.secondary)
                }
                .listRowBackground(Palette.panel)
            }
        }
    }

    private func footer(_ listing: NoteListing) -> String? {
        var parts: [String] = []
        if listing.hidden > 0 { parts.append("\(listing.hidden) hidden by the filter") }
        if listing.internalFiles > 0 { parts.append("\(listing.internalFiles) of the canvas's own files hidden") }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private func row(_ entry: NoteEntry) -> some View {
        NavigationLink(value: entry.kind == .folder ? Route.folder(entry.path, entry.name) : Route.note(entry.path)) {
            NoteEntryRow(entry: entry)
        }
        .listRowBackground(Palette.panel)
        .swipeActions(edge: .trailing) {
            Button(role: .destructive) { deleting = entry } label: { Label("Delete", systemImage: "trash") }
            Button {
                newName = entry.name
                renaming = entry
            } label: { Label("Rename", systemImage: "pencil") }
                .tint(Palette.finished)
        }
        .swipeActions(edge: .leading) {
            if entry.kind.reviewable && !entry.provenance.reviewed {
                Button { markReviewed(entry) } label: { Label("Reviewed", systemImage: "checkmark") }.tint(Palette.finished)
            }
        }
        .contextMenu {
            if entry.kind.reviewable && !entry.provenance.reviewed {
                Button { markReviewed(entry) } label: { Label("Mark reviewed", systemImage: "checkmark") }
            }
            Button {
                newName = entry.name
                renaming = entry
            } label: { Label("Rename", systemImage: "pencil") }
            Button { UIPasteboard.general.string = entry.path } label: { Label("Copy path", systemImage: "doc.on.doc") }
            Button(role: .destructive) { deleting = entry } label: { Label("Delete", systemImage: "trash") }
        }
    }

    @ViewBuilder
    private var results: some View {
        Section {
            if hits.isEmpty {
                Text(searching ? "Searching…" : "No notes contain “\(query)”.")
                    .foregroundStyle(Palette.muted)
                    .listRowBackground(Palette.panel)
            }
            ForEach(hits) { hit in
                NavigationLink(value: Route.note(hit.path)) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(fileName(hit.path)).font(.body.weight(.medium)).lineLimit(1)
                        Text(hit.line == 0 ? relative(hit.path) : "line \(hit.line): \(hit.text)").font(.caption).foregroundStyle(Palette.gray).lineLimit(2)
                    }
                }
                .listRowBackground(Palette.panel)
                .accessibilityIdentifier("noteHit")
            }
        } header: {
            Text("Search")
        }
    }

    private func fileName(_ path: String) -> String {
        path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? path
    }

    private func relative(_ path: String) -> String {
        for root in roots where path.lowercased().hasPrefix(root.path.lowercased()) {
            return root.label + String(path.dropFirst(root.path.count)).replacingOccurrences(of: "\\", with: "/")
        }
        return path
    }

    // MARK: Service calls

    private func load() async {
        guard connection.state == .online else { return }
        let folder = folder, filter = NoteFilter(stored: storedFilter), internalFiles = showInternal
        do {
            listing = try await connection.perform { try $0.listNotes(folder: folder, filter: filter, showInternal: internalFiles) }
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func search() async {
        let text = query.trimmingCharacters(in: .whitespaces)
        guard text.count >= 2 else {
            hits = []
            return
        }
        searching = true
        try? await Task.sleep(nanoseconds: 350_000_000)
        guard !Task.isCancelled else { return }
        let found = try? await connection.perform { try $0.searchNotes(query: text, limit: 60) }
        guard !Task.isCancelled else { return }
        // Names first, then lines; one row per line.
        hits = (found ?? []).sorted { ($0.line == 0 ? 0 : 1, $0.path) < ($1.line == 0 ? 0 : 1, $1.path) }
        searching = false
    }

    private func make(_ what: NoteCreate) {
        let name = newName.trimmingCharacters(in: .whitespaces)
        newName = ""
        guard !name.isEmpty else { return }
        let folder = folder
        Task {
            do {
                if what == .folder {
                    let path = try await connection.perform { try $0.createFolder(folder: folder, name: name) }
                    await load()
                    push(.folder(path, fileName(path)))
                } else {
                    let path = try await connection.perform { try $0.createNote(folder: folder, name: name, text: "") }
                    await load()
                    push(.note(path, edit: true))
                }
            } catch {
                connection.show(error)
            }
        }
    }

    private func rename() {
        guard let entry = renaming else { return }
        let name = newName
        newName = ""
        Task {
            do {
                _ = try await connection.perform { try $0.renameNote(path: entry.path, name: name) }
                await load()
            } catch {
                connection.show(error)
            }
        }
    }

    private func trash(_ entry: NoteEntry) {
        Task {
            do {
                try await connection.perform { try $0.deleteNote(path: entry.path) }
                connection.toast = Toast(text: "\(entry.name) is in the trash.", error: false)
                await load()
            } catch {
                connection.show(error)
            }
        }
    }

    private func markReviewed(_ entry: NoteEntry) {
        Task {
            do {
                try await connection.perform { try $0.markNoteReviewed(path: entry.path) }
                await load()
            } catch {
                connection.show(error)
            }
        }
    }
}

/// One notebook entry: a folder, or a note with whom it is for and who wrote
/// it (notes for agents dimmed, as on the desktop).
struct NoteEntryRow: View {
    let entry: NoteEntry

    var body: some View {
        let p = entry.provenance
        let folder = entry.kind == .folder
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: entry.kind.symbol)
                .foregroundStyle(folder ? Palette.accent : p.forYou ? Palette.positive : Palette.purple)
                .frame(width: 22)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(folder ? entry.name : entry.title)
                        .font(.body.weight(folder || (p.forYou && !p.reviewed && p.author != .human) ? .semibold : .regular))
                        .italic(!folder && !p.forYou)
                        .foregroundStyle(Palette.text)
                        .lineLimit(2)
                    Spacer(minLength: 4)
                    if !folder && entry.modifiedMs > 0 {
                        Text(Ago.short(entry.modifiedMs)).font(.caption2).foregroundStyle(Palette.muted)
                    }
                }
                if !folder {
                    Text(subtitle).font(.caption).foregroundStyle(Palette.gray).lineLimit(2)
                }
            }
        }
        .padding(.vertical, 2)
        .opacity(!folder && !p.forYou ? 0.62 : 1)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(folder ? "folderRow" : "noteRow")
        .accessibilityLabel(folder ? "Folder \(entry.name)" : "\(entry.title), \(subtitle)")
    }

    private var subtitle: String {
        var line = entry.provenance.summary()
        if entry.title != entry.name && entry.title + ".md" != entry.name {
            line = "\(entry.name) · " + line
        }
        return line
    }
}
