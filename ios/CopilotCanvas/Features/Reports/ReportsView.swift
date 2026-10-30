import SwiftUI

extension ReportThread: Identifiable {}
extension ReportItem: Identifiable {}
extension RefusedReport: Identifiable {}

extension ReportItem {
    /// Its kind's colour (the desktop's).
    var kindColor: Color {
        switch kind {
        case "ok", "delivered": return Palette.positive
        case "warn": return Palette.attention
        case "alert", "failed": return Palette.failed
        case "queued": return Palette.muted
        default: return Palette.gray
        }
    }

    var formatLabel: String {
        switch format {
        case .html: return "HTML"
        case .markdown: return "Markdown"
        case .message: return "message"
        }
    }

    /// "warn · from Drift guard (Payments) · 12m ago · HTML".
    var details: String {
        var parts: [String] = []
        if kind != "info" && !kind.isEmpty { parts.append(kind) }
        if !sourceName.isEmpty { parts.append(groupName.isEmpty ? "from \(sourceName)" : "from \(sourceName) (\(groupName))") }
        if createdMs > 0 { parts.append("\(Ago.short(createdMs)) ago") }
        parts.append(formatLabel)
        return parts.joined(separator: " · ")
    }
}

enum Ago {
    /// "now", "5m", "2h", "3d".
    static func short(_ ms: UInt64) -> String {
        let seconds = Date().timeIntervalSince1970 - Double(ms) / 1000
        switch seconds {
        case ..<60: return "now"
        case ..<3600: return "\(Int(seconds / 60))m"
        case ..<86400: return "\(Int(seconds / 3600))h"
        default: return "\(Int(seconds / 86400))d"
        }
    }
}

/// The machine's reports inbox: what agents published (HTML and Markdown
/// reports, short messages), one row per thread, unread first in your eye.
/// Reading one never starts or resumes anything.
struct ReportsView: View {
    @ObservedObject var connection: ServiceConnection
    /// Opens a running session's terminal (a report's "View session").
    var openSession: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var path: [String]
    @State private var deleting: ReportThread?

    init(connection: ServiceConnection, open report: String? = nil, openSession: @escaping (String) -> Void) {
        self.connection = connection
        self.openSession = openSession
        _path = State(initialValue: report.map { [$0] } ?? [])
    }

    var body: some View {
        NavigationStack(path: $path) {
            content
                .background(Palette.background)
                .navigationTitle("Reports")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
                    ToolbarItem(placement: .topBarLeading) {
                        if (connection.reports?.unread ?? 0) > 0 {
                            Button {
                                act { try $0.markAllReportsRead() }
                            } label: { Label("Mark all read", systemImage: "checkmark.circle") }
                        }
                    }
                }
                .navigationDestination(for: String.self) { id in
                    ReportViewer(connection: connection, reportId: id) { session in
                        dismiss()
                        openSession(session)
                    }
                }
        }
        .confirmationDialog(deleteTitle, isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }), titleVisibility: .visible, presenting: deleting) { thread in
            Button("Delete", role: .destructive) { act { try $0.deleteReports(ids: thread.items.map(\.id)) } }
        } message: { _ in
            Text("It goes from the inbox in every window and on every phone; this cannot be undone.")
        }
    }

    private var deleteTitle: String {
        guard let deleting else { return "" }
        return deleting.items.count == 1 ? "Delete “\(deleting.latest.title)”?" : "Delete the \(deleting.items.count) reports of “\(deleting.latest.title)”?"
    }

    @ViewBuilder
    private var content: some View {
        if let reports = connection.reports, reports.loaded {
            if reports.threads.isEmpty && reports.refused.isEmpty {
                ContentUnavailableView("No reports yet", systemImage: "tray", description: Text("Agents publish reports and messages here with `copilot-canvas report publish`."))
            } else {
                list(reports)
            }
        } else if connection.state == .online {
            VStack(spacing: 12) {
                ProgressView()
                Text("Waiting for the service's inbox…").foregroundStyle(.secondary)
                Text("An older Copilot Canvas service sends none: update it on \(connection.server.name).").font(.caption).foregroundStyle(Palette.muted).multilineTextAlignment(.center)
            }
            .padding()
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            ContentUnavailableView("Offline", systemImage: "wifi.exclamationmark", description: Text("The inbox shows once \(connection.server.name) is connected."))
        }
    }

    private func list(_ reports: ReportsOverview) -> some View {
        List {
            SwiftUI.Section {
                ForEach(reports.threads) { thread in
                    NavigationLink(value: thread.id) {
                        ReportRow(thread: thread)
                    }
                    .listRowBackground(thread.unread > 0 ? Palette.panelHi : Palette.panel)
                    .swipeActions(edge: .leading) {
                        let ids = thread.items.map(\.id)
                        if thread.unread > 0 {
                            Button { act { try $0.markReports(ids: ids, read: true) } } label: { Label("Read", systemImage: "envelope.open") }.tint(Palette.finished)
                        } else {
                            Button { act { try $0.markReports(ids: [thread.id], read: false) } } label: { Label("Unread", systemImage: "envelope.badge") }.tint(Palette.attention)
                        }
                    }
                    .swipeActions(edge: .trailing) {
                        Button(role: .destructive) { deleting = thread } label: { Label("Delete", systemImage: "trash") }
                    }
                    .contextMenu {
                        if thread.unread > 0 {
                            Button { act { try $0.markReports(ids: thread.items.map(\.id), read: true) } } label: { Label("Mark read", systemImage: "envelope.open") }
                        } else {
                            Button { act { try $0.markReports(ids: [thread.id], read: false) } } label: { Label("Mark unread", systemImage: "envelope.badge") }
                        }
                        Button(role: .destructive) { deleting = thread } label: { Label("Delete", systemImage: "trash") }
                    }
                }
            } header: {
                Text(summary(reports))
            }
            if !reports.refused.isEmpty {
                SwiftUI.Section("Refused by the service") {
                    ForEach(reports.refused) { refused in
                        VStack(alignment: .leading, spacing: 3) {
                            Label(refused.title, systemImage: "xmark.octagon.fill").font(.subheadline.weight(.semibold)).foregroundStyle(Palette.failed)
                            Text(refused.reason).font(.caption).foregroundStyle(Palette.gray)
                        }
                        .listRowBackground(Palette.panel)
                        .swipeActions(edge: .trailing) {
                            Button(role: .destructive) { act { try $0.deleteReports(ids: [refused.id]) } } label: { Label("Dismiss", systemImage: "xmark") }
                        }
                    }
                }
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
    }

    private func summary(_ reports: ReportsOverview) -> String {
        let count = reports.threads.count
        if reports.unread > 0 { return "\(reports.unread) unread · \(count) in the inbox" }
        return count == 1 ? "1 report · all read" : "\(count) reports · all read"
    }

    private func act(_ work: (Client) throws -> Void) {
        do { try work(connection.client) } catch { connection.show(error) }
    }
}

/// One inbox row: unread dot or kind, title, age; who, kind, updates, the first line.
struct ReportRow: View {
    let thread: ReportThread

    var body: some View {
        let item = thread.latest
        HStack(alignment: .top, spacing: 10) {
            Group {
                if thread.unread > 0 {
                    Circle().fill(Palette.attention).frame(width: 9, height: 9).padding(.top, 6)
                } else {
                    Text(item.glyph).font(.callout.weight(.semibold)).foregroundStyle(item.kindColor)
                }
            }
            .frame(width: 16)
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(item.title).font(.body.weight(thread.unread > 0 ? .semibold : .regular)).foregroundStyle(Palette.text).lineLimit(2)
                    Spacer(minLength: 4)
                    Text(Ago.short(item.createdMs)).font(.caption2).foregroundStyle(Palette.muted)
                }
                Text(subtitle(item)).font(.caption).foregroundStyle(Palette.gray).lineLimit(2)
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("report-row")
        .accessibilityLabel("\(item.title)\(thread.unread > 0 ? ", unread" : ""), \(subtitle(item))")
    }

    private func subtitle(_ item: ReportItem) -> String {
        var parts: [String] = []
        if !item.sourceName.isEmpty { parts.append(item.sourceName) }
        if item.kind != "info" && !item.kind.isEmpty { parts.append(item.kind) }
        if thread.items.count > 1 { parts.append("\(thread.items.count) updates") }
        if !item.preview.isEmpty { parts.append(item.preview) }
        return parts.joined(separator: " · ")
    }
}
