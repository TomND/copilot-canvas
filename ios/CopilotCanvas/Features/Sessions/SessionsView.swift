import SwiftUI

/// Sessions by what they need: waiting for you, ready for review, then by group.
struct SessionsView: View {
    @ObservedObject var connection: ServiceConnection
    var open: (String) -> Void
    @State private var filter = ""
    @State private var renaming: SessionItem?
    @State private var newName = ""

    private struct Section: Identifiable {
        let id: String
        let title: String
        let color: Color?
        let items: [SessionItem]
    }

    var body: some View {
        Group {
            if let overview = connection.overview, !overview.sessions.isEmpty {
                list(overview)
            } else if connection.overview == nil {
                VStack(spacing: 12) {
                    ProgressView()
                    Text("Loading the canvas…").foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView("No sessions on the canvas", systemImage: "square.dashed", description: Text("Start one with +."))
            }
        }
        .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField("Name", text: $newName)
            Button("Rename") {
                if let item = renaming {
                    do { try connection.client.renameSession(id: item.id, name: newName) } catch { connection.show(error) }
                }
                renaming = nil
            }
            Button("Cancel", role: .cancel) { renaming = nil }
        }
    }

    private func list(_ overview: Overview) -> some View {
        List {
            ForEach(sections(overview)) { section in
                SwiftUI.Section {
                    ForEach(section.items, id: \.id) { item in
                        row(item, overview: overview)
                    }
                } header: {
                    HStack(spacing: 6) {
                        if let color = section.color {
                            RoundedRectangle(cornerRadius: 2).fill(color).frame(width: 8, height: 8)
                        }
                        Text(section.title)
                        Spacer()
                        Text("\(section.items.count)").foregroundStyle(.secondary)
                    }
                }
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .searchable(text: $filter, placement: .navigationBarDrawer(displayMode: .automatic), prompt: "Filter sessions")
        .refreshable { connection.reconnectNow() }
    }

    @ViewBuilder
    private func row(_ item: SessionItem, overview: Overview) -> some View {
        Button {
            if item.kind == .subagent, let parent = item.parent {
                open(parent)
            } else if item.kind == .session {
                open(item.id)
            }
        } label: {
            SessionRow(item: item, nested: item.parent != nil && item.kind == .session)
        }
        .buttonStyle(.plain)
        .foregroundStyle(Palette.text)
        .listRowBackground(item.status.wantsAttention ? (item.status == .needsYou ? Palette.attentionBg : Palette.finishedBg) : Palette.panel)
        .swipeActions(edge: .trailing) {
            if item.alive {
                Button { connection.client.stopSession(id: item.id) } label: { Label("Stop", systemImage: "stop.fill") }.tint(Palette.failed)
            } else if item.kind == .session {
                Button(role: .destructive) {
                    do { try connection.client.removeSession(id: item.id) } catch { connection.show(error) }
                } label: { Label("Remove", systemImage: "trash") }
            }
        }
        .swipeActions(edge: .leading) {
            if item.status == .done {
                Button { connection.client.markReviewed(id: item.id) } label: { Label("Reviewed", systemImage: "checkmark") }.tint(Palette.finished)
            }
        }
        .contextMenu {
            if item.kind == .session {
                Button { open(item.id) } label: { Label(item.alive ? "Open terminal" : "Start / resume", systemImage: "terminal") }
                Button {
                    newName = item.name
                    renaming = item
                } label: { Label("Rename", systemImage: "pencil") }
                if item.status == .done {
                    Button { connection.client.markReviewed(id: item.id) } label: { Label("Mark reviewed", systemImage: "checkmark") }
                }
                if !item.cwd.isEmpty {
                    Button { UIPasteboard.general.string = item.cwd } label: { Label("Copy folder path", systemImage: "doc.on.doc") }
                }
                if item.alive {
                    Button(role: .destructive) { connection.client.stopSession(id: item.id) } label: { Label("Stop", systemImage: "stop.fill") }
                }
            }
        }
    }

    private func sections(_ overview: Overview) -> [Section] {
        let query = filter.trimmingCharacters(in: .whitespaces).lowercased()
        let items = overview.sessions.filter { item in
            item.kind != .automation && (query.isEmpty || item.name.lowercased().contains(query) || item.cwd.lowercased().contains(query))
        }
        let groupsById = Dictionary(uniqueKeysWithValues: overview.groups.map { ($0.id, $0) })
        // Sessions of a parent come right after it; subagents are shown inside their session's row.
        let sessions = items.filter { $0.kind != .subagent }
        var sections: [Section] = []
        let waiting = sessions.filter { $0.status == .needsYou }
        if !waiting.isEmpty { sections.append(Section(id: "waiting", title: "Needs you", color: Palette.attention, items: waiting)) }
        let review = sessions.filter { $0.status == .done }
        if !review.isEmpty { sections.append(Section(id: "review", title: "Ready for review", color: Palette.finished, items: review)) }
        let rest = sessions.filter { !$0.status.wantsAttention }
        for group in overview.groups {
            let members = ordered(rest.filter { $0.group == group.id })
            if !members.isEmpty {
                sections.append(Section(id: group.id, title: group.name, color: Palette.group(group.color), items: members))
            }
        }
        let loose = ordered(rest.filter { $0.group.flatMap { groupsById[$0] } == nil })
        if !loose.isEmpty { sections.append(Section(id: "loose", title: "Ungrouped", color: nil, items: loose)) }
        return sections
    }

    /// Running first, then by last activity; subsessions right after their parent.
    private func ordered(_ items: [SessionItem]) -> [SessionItem] {
        let sorted = items.sorted { a, b in
            if a.status.rank != b.status.rank { return a.status.rank < b.status.rank }
            return a.lastActivityMs > b.lastActivityMs
        }
        let ids = Set(sorted.map(\.id))
        var out: [SessionItem] = []
        for item in sorted where item.parent.map({ !ids.contains($0) }) ?? true {
            out.append(item)
            out.append(contentsOf: sorted.filter { $0.parent == item.id })
        }
        return out
    }
}

/// One session: status, name, what it is doing, its subagents.
struct SessionRow: View {
    let item: SessionItem
    var nested = false

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            StatusIcon(status: item.status).padding(.top, 2)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if nested { Text("↳").foregroundStyle(Palette.muted) }
                    Text(item.name).font(.body.weight(.semibold)).lineLimit(1)
                    if item.kind == .passive { Image(systemName: "arrow.triangle.2.circlepath").font(.caption).foregroundStyle(Palette.loop) }
                    Spacer(minLength: 4)
                    Text(ago(item.lastActivityMs)).font(.caption2).foregroundStyle(Palette.muted)
                }
                HStack(spacing: 6) {
                    Text(item.statusLabel).font(.caption.weight(.medium)).foregroundStyle(item.status.color)
                    if !item.subtitle.isEmpty {
                        Text("·").foregroundStyle(Palette.muted)
                        Text(item.subtitle).font(.caption).foregroundStyle(Palette.gray).lineLimit(1)
                    }
                }
                HStack(spacing: 8) {
                    AgentBadge(agent: item.agent)
                    if !item.branch.isEmpty {
                        Label(item.branch, systemImage: "arrow.triangle.branch").font(.caption2).foregroundStyle(Palette.muted).lineLimit(1)
                    }
                    if !item.loopLabel.isEmpty {
                        Label(item.loopLabel, systemImage: "repeat").font(.caption2).foregroundStyle(Palette.loop)
                    }
                    if item.mailbox > 0 {
                        Label("\(item.mailbox)", systemImage: "envelope.fill").font(.caption2).foregroundStyle(Palette.attention)
                    }
                }
                if !item.subagents.isEmpty {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(item.subagents.prefix(4), id: \.key) { agent in
                            HStack(spacing: 6) {
                                Image(systemName: agent.status.symbol).font(.caption2).foregroundStyle(agent.status.color)
                                Text(agent.name).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            }
                        }
                        if item.subagents.count > 4 {
                            Text("+\(item.subagents.count - 4) more subagents").font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .padding(.leading, 4)
                    .padding(.top, 2)
                }
            }
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
    }

    private func ago(_ ms: UInt64) -> String {
        guard ms > 0 else { return "" }
        let seconds = Date().timeIntervalSince1970 - Double(ms) / 1000
        switch seconds {
        case ..<60: return "now"
        case ..<3600: return "\(Int(seconds / 60))m"
        case ..<86400: return "\(Int(seconds / 3600))h"
        default: return "\(Int(seconds / 86400))d"
        }
    }
}

struct StatusIcon: View {
    let status: StatusKind
    @State private var spin = false

    var body: some View {
        Image(systemName: status.symbol)
            .font(.system(size: 16, weight: .semibold))
            .foregroundStyle(status.color)
            .rotationEffect(.degrees(status.animated && spin ? 360 : 0))
            .animation(status.animated ? .linear(duration: 1.6).repeatForever(autoreverses: false) : .default, value: spin)
            .onAppear { spin = status.animated }
            .onChange(of: status) { _, new in spin = new.animated }
            .frame(width: 22)
            .accessibilityLabel(Text(String(describing: status)))
    }
}

struct AgentBadge: View {
    let agent: String

    var body: some View {
        Text(label)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Palette.panelHi, in: Capsule())
            .foregroundStyle(Palette.text)
    }

    private var label: String {
        switch agent {
        case "opencode": return "OpenCode"
        case "claude": return "Claude"
        default: return "Copilot"
        }
    }
}
