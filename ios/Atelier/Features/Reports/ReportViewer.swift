import SwiftUI

/// One report, as its page in the locked-down web view, with who published
/// it, its pull request and links, and the other reports of its thread.
/// Opening it marks its thread read; nothing is ever started.
struct ReportViewer: View {
    @ObservedObject var connection: ServiceConnection
    /// The report opened (a thread's row opens its newest).
    let reportId: String
    /// Shows a running session's terminal.
    var openSession: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var shown: String?
    @State private var controller: ReportWebController?
    @State private var text = ""
    @State private var error: String?
    @State private var link: URL?
    @State private var confirmDelete = false

    private var current: String { shown ?? reportId }
    private var thread: ReportThread? { connection.reports?.threads.first { $0.items.contains { $0.id == current } } }
    private var item: ReportItem? { thread?.items.first { $0.id == current } }

    var body: some View {
        VStack(spacing: 0) {
            if let item {
                header(item)
            }
            ZStack {
                Color(rgb: TerminalColors.background)
                if connection.reports?.loaded == true && item == nil {
                    ContentUnavailableView("This report is no longer in the inbox", systemImage: "tray", description: Text("It was deleted in another window or on another phone."))
                } else if let controller {
                    ReportWebView(controller: controller).id(ObjectIdentifier(controller))
                } else if let error {
                    ContentUnavailableView("Cannot show this report", systemImage: "doc.questionmark", description: Text(error))
                } else {
                    ProgressView("Loading the report…")
                }
            }
        }
        .background(Palette.background)
        .navigationTitle(item?.title ?? "Report")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) { menu }
            if let thread, thread.items.count > 1, let index = thread.items.firstIndex(where: { $0.id == current }) {
                ToolbarItemGroup(placement: .bottomBar) {
                    Button { shown = thread.items[index + 1].id } label: { Label("Older", systemImage: "chevron.left") }
                        .disabled(index + 1 >= thread.items.count)
                    Spacer()
                    Text("\(index + 1) of \(thread.items.count) · newest first").font(.caption).foregroundStyle(Palette.muted)
                    Spacer()
                    Button { shown = thread.items[index - 1].id } label: { Label("Newer", systemImage: "chevron.right") }
                        .disabled(index == 0)
                }
            }
        }
        .task(id: current) { await load() }
        .confirmationDialog("Open this link from a report?", isPresented: Binding(get: { link != nil }, set: { if !$0 { link = nil } }), titleVisibility: .visible, presenting: link) { url in
            Button(url.scheme == "mailto" ? "Write the email" : "Open in Safari") { openURL(url) }
        } message: { url in
            Text(url.absoluteString)
        }
        .confirmationDialog("Delete “\(item?.title ?? "this report")”?", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("Delete", role: .destructive) {
                do {
                    try connection.client.deleteReports(ids: [current])
                    dismiss()
                } catch {
                    connection.show(error)
                }
            }
        } message: {
            Text("It goes from the inbox in every window and on every phone; this cannot be undone.")
        }
    }

    private func header(_ item: ReportItem) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(item.glyph).foregroundStyle(item.kindColor)
                Text(item.details).font(.caption).foregroundStyle(Palette.gray).lineLimit(2)
            }
            let actions = chips(item)
            if !actions.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(actions, id: \.title) { chip in
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

    private struct Chip {
        let title: String
        let symbol: String
        let action: () -> Void
    }

    /// The sessions it names (only running ones: viewing starts nothing), its
    /// pull request and links (checked addresses: they open at once).
    private func chips(_ item: ReportItem) -> [Chip] {
        var out: [Chip] = []
        for box in [item.sourceBox, item.sessionBox] where !box.isEmpty {
            if let session = connection.overview?.sessions.first(where: { $0.id == box && $0.kind == .session && $0.alive }) {
                out.append(Chip(title: "View “\(session.name)”", symbol: "terminal") { openSession(session.id) })
            }
        }
        if reportLinkAllowed(url: item.pr), let url = URL(string: item.pr) {
            out.append(Chip(title: "Pull request", symbol: "arrow.triangle.pull") { openURL(url) })
        }
        for link in item.links {
            if let url = URL(string: link.url) {
                out.append(Chip(title: link.title, symbol: "link") { openURL(url) })
            }
        }
        return out
    }

    private var menu: some View {
        Menu {
            if !text.isEmpty {
                Button {
                    UIPasteboard.general.string = text
                    connection.toast = Toast(text: "Copied the report", error: false)
                } label: { Label("Copy text", systemImage: "doc.on.doc") }
                ShareLink(item: text, subject: Text(item?.title ?? "Report")) { Label("Share text", systemImage: "square.and.arrow.up") }
            }
            if item?.unread == false {
                Button {
                    do {
                        try connection.client.markReports(ids: [current], read: false)
                        dismiss()
                    } catch {
                        connection.show(error)
                    }
                } label: { Label("Mark unread", systemImage: "envelope.badge") }
            }
            Button(role: .destructive) { confirmDelete = true } label: { Label("Delete", systemImage: "trash") }
        } label: {
            Image(systemName: "ellipsis.circle")
        }
        .accessibilityLabel("Report actions")
    }

    private func load() async {
        controller = nil
        error = nil
        text = ""
        let id = current
        // Opening a thread reads it (on every window and phone).
        let unread = thread?.items.filter(\.unread).map(\.id) ?? []
        if !unread.isEmpty { try? connection.client.markReports(ids: unread, read: true) }
        do {
            let rules = try await ReportLockdown.ruleList()
            let page = try await connection.perform { try $0.readReport(id: id) }
            guard id == current else { return }
            let made = ReportWebController(rules: rules)
            made.onLink = { url in link = url }
            made.onFinish = { [weak made] in
                Task { @MainActor in text = await made?.text() ?? "" }
            }
            made.load(page.html)
            controller = made
        } catch {
            if id == current { self.error = describe(error) }
        }
    }
}
