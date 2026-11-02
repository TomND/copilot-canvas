import SwiftUI

/// A note as its page in the locked-down web view (the core's sanitized
/// page: Markdown rendered, HTML with its scripts off, pictures inline), with
/// whom it is for and who wrote it. `#note:` links open notes here, `#todo:`
/// links todos, web links ask before Safari. Edit opens its source.
///
/// Unlike the desktop, which can let one HTML note run its scripts, the
/// phone never runs a note's scripts.
struct NoteView: View {
    @ObservedObject var connection: ServiceConnection
    let path: String
    /// Open the source editor at once (a note just created).
    var editOnOpen = false
    var push: (Route) -> Void
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var shown: String?
    @State private var page: NotePage?
    @State private var controller: ReportWebController?
    @State private var error: String?
    @State private var text = ""
    @State private var link: URL?
    @State private var editing = false
    @State private var openedEditor = false
    @State private var confirmDelete = false
    @State private var renaming = false
    @State private var newName = ""
    @State private var gone = false

    private var current: String { shown ?? path }
    private var name: String { current.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? current }

    var body: some View {
        VStack(spacing: 0) {
            if let page { header(page) }
            if gone {
                Label("This note was moved or deleted on \(connection.server.name).", systemImage: "exclamationmark.triangle.fill")
                    .font(.footnote)
                    .foregroundStyle(Palette.attention)
                    .padding(8)
                    .frame(maxWidth: .infinity)
                    .background(Palette.attentionBg)
            }
            ZStack {
                Color(rgb: TerminalColors.background)
                if let controller {
                    ReportWebView(controller: controller).id(ObjectIdentifier(controller))
                } else if let error {
                    ContentUnavailableView("Cannot show this note", systemImage: "doc.questionmark", description: Text(error))
                } else {
                    ProgressView("Loading the note…")
                }
            }
        }
        .background(Palette.background)
        .navigationTitle(page?.title ?? name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                if page?.kind.editable == true {
                    Button("Edit") { editing = true }
                        .disabled(connection.state != .online)
                        .accessibilityIdentifier("editNote")
                }
                menu
            }
        }
        .task(id: current) { await load() }
        .task(id: "poll \(current)") { await poll() }
        .onAppear {
            if editOnOpen && !openedEditor {
                openedEditor = true
                // After the push (and the name dialog's dismissal) settle.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { editing = true }
            }
        }
        .fullScreenCover(isPresented: $editing) {
            NoteEditor(connection: connection, path: current, newNote: editOnOpen) {
                Task { await load() }
            }
        }
        .confirmationDialog("Open this link from a note?", isPresented: Binding(get: { link != nil }, set: { if !$0 { link = nil } }), titleVisibility: .visible, presenting: link) { url in
            Button(url.scheme == "mailto" ? "Write the email" : "Open in Safari") { openURL(url) }
        } message: { url in
            Text(url.absoluteString)
        }
        .confirmationDialog("Delete “\(name)”?", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("Move to the trash", role: .destructive) { trash() }
        } message: {
            Text("It moves to the notebook's .trash folder on \(connection.server.name), where it can be taken back.")
        }
        .alert("Rename", isPresented: $renaming) {
            TextField("Name", text: $newName)
            Button("Rename") { rename() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("It keeps its extension when you type none.")
        }
    }

    private func header(_ page: NotePage) -> some View {
        let p = page.provenance
        return VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Image(systemName: p.forYou ? "circle.fill" : "diamond")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(p.forYou ? Palette.positive : Palette.purple)
                    .accessibilityHidden(true)
                Text(details(p)).font(.caption).foregroundStyle(Palette.gray).lineLimit(3)
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("noteProvenance")
            let chips = self.chips(page)
            if !chips.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(chips, id: \.title) { chip in
                            Button(action: chip.action) {
                                Label(chip.title, systemImage: chip.symbol)
                                    .font(.caption.weight(.medium))
                                    .lineLimit(1)
                                    .padding(.horizontal, 10)
                                    .padding(.vertical, 5)
                                    .background(Palette.panelHi, in: Capsule())
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(Palette.accent)
                            .accessibilityIdentifier(chip.id)
                        }
                    }
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Palette.panel)
        .overlay(alignment: .bottom) { Rectangle().fill(Palette.border).frame(height: 0.5) }
    }

    /// "For you · written by Copilot · session Fix flaky test · created … · reviewed".
    private func details(_ p: NoteProvenance) -> String {
        var parts = ["\(p.audienceText)\(p.inferred ? " (inferred)" : "")", p.writer]
        if !p.sessionName.isEmpty { parts.append("session \(p.sessionName)") } else if !p.session.isEmpty { parts.append("session \(p.session)") }
        if !p.created.isEmpty { parts.append(String(p.created.prefix(16)).replacingOccurrences(of: "T", with: " ")) }
        if p.reviewed { parts.append("reviewed ✓") }
        return parts.joined(separator: " · ")
    }

    private struct Chip {
        let id: String
        let title: String
        let symbol: String
        let action: () -> Void
    }

    /// Mark reviewed; the writing session when it runs (viewing starts nothing).
    private func chips(_ page: NotePage) -> [Chip] {
        var out: [Chip] = []
        if page.kind.reviewable && !page.provenance.reviewed {
            out.append(Chip(id: "markReviewed", title: "Mark reviewed", symbol: "checkmark.circle") { markReviewed() })
        }
        let session = page.provenance.session
        if !session.isEmpty, let item = connection.overview?.sessions.first(where: { $0.id == session && $0.kind == .session && $0.alive }) {
            out.append(Chip(id: "viewSession", title: "View “\(item.name)”", symbol: "terminal") { push(.terminal(item.id)) })
        }
        return out
    }

    private var menu: some View {
        Menu {
            if !text.isEmpty {
                Button {
                    UIPasteboard.general.string = text
                    connection.toast = Toast(text: "Copied the note", error: false)
                } label: { Label("Copy text", systemImage: "doc.on.doc") }
                ShareLink(item: text, subject: Text(page?.title ?? name)) { Label("Share text", systemImage: "square.and.arrow.up") }
            }
            if let page, page.kind.reviewable, !page.provenance.reviewed {
                Button { markReviewed() } label: { Label("Mark reviewed", systemImage: "checkmark.circle") }
            }
            Button {
                newName = name
                renaming = true
            } label: { Label("Rename…", systemImage: "pencil") }
            Button { UIPasteboard.general.string = current } label: { Label("Copy path", systemImage: "link") }
            Button(role: .destructive) { confirmDelete = true } label: { Label("Delete…", systemImage: "trash") }
        } label: {
            Image(systemName: "ellipsis.circle")
        }
        .accessibilityLabel("Note actions")
        .accessibilityIdentifier("noteActions")
    }

    // MARK: Loading

    private func load() async {
        let path = current
        do {
            let rules = try await ReportLockdown.ruleList()
            let loaded = try await connection.perform { try $0.readNote(path: path) }
            guard path == current else { return }
            let made = controller ?? ReportWebController(rules: rules, identifier: "notePage")
            made.onLink = { url in link = url }
            made.onAppLink = { target in follow(target) }
            made.onFinish = { [weak made] in
                Task { @MainActor in text = await made?.text() ?? "" }
            }
            if loaded.html != page?.html || controller == nil { made.load(loaded.html) }
            page = loaded
            controller = made
            error = nil
            gone = false
        } catch {
            if path == current && page == nil { self.error = describe(error) }
        }
    }

    /// Changes made elsewhere (an agent, the desk) show while you read.
    private func poll() async {
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            guard !Task.isCancelled, !editing, connection.state == .online, let page else { continue }
            let path = current
            guard let revs = try? await connection.perform({ try $0.noteRevisions(paths: [path]) }), let rev = revs.first else { continue }
            if let rev, rev != page.rev {
                await load()
            } else if rev == nil {
                gone = true
            }
        }
    }

    private func follow(_ target: NoteLink) {
        switch target {
        case .todo(let id):
            push(.todo(id))
        case .note(let name):
            Task {
                do {
                    if let found = try await connection.perform({ try $0.resolveNote(name: name) }) {
                        push(.note(found))
                    } else {
                        connection.toast = Toast(text: "No note named “\(name)” in the notebook.", error: true)
                    }
                } catch {
                    connection.show(error)
                }
            }
        }
    }

    private func markReviewed() {
        let path = current
        Task {
            do {
                try await connection.perform { try $0.markNoteReviewed(path: path) }
                await load()
            } catch {
                connection.show(error)
            }
        }
    }

    private func rename() {
        let path = current, name = newName
        Task {
            do {
                shown = try await connection.perform { try $0.renameNote(path: path, name: name) }
            } catch {
                connection.show(error)
            }
        }
    }

    private func trash() {
        let path = current
        Task {
            do {
                try await connection.perform { try $0.deleteNote(path: path) }
                connection.toast = Toast(text: "\(name) is in the trash.", error: false)
                dismiss()
            } catch {
                connection.show(error)
            }
        }
    }
}
