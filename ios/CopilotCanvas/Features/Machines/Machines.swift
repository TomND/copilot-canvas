import SwiftUI

/// The machine shown, as a menu: every paired machine with its state and
/// what needs you there, pairing another one, settings.
struct MachineMenu: View {
    @EnvironmentObject private var model: AppModel
    var showPair: () -> Void
    var showSettings: () -> Void

    var body: some View {
        Menu {
            Section("Machines") {
                ForEach(model.servers) { server in
                    Button {
                        model.select(server.id)
                    } label: {
                        Label(title(server), systemImage: server.id == model.currentID ? "checkmark.circle.fill" : symbol(server))
                    }
                }
            }
            Button(action: showPair) { Label("Pair another machine", systemImage: "plus") }
            Button(action: showSettings) { Label("Settings", systemImage: "gearshape") }
        } label: {
            HStack(spacing: 6) {
                Circle()
                    .fill(Palette.group(UInt8(model.current?.colorIndex ?? 0)))
                    .frame(width: 9, height: 9)
                    .overlay(Circle().stroke(stateColor(model.connection?.state), lineWidth: model.connection?.state == .online ? 0 : 2).frame(width: 13, height: 13))
                Text(model.current?.name ?? "Machines")
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                    .frame(maxWidth: 110, alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
                let others = model.servers.filter { $0.id != model.currentID }.map { model.needsYou($0.id) }.reduce(0, +)
                if others > 0 {
                    Text("\(others)")
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Palette.attention, in: Capsule())
                        .foregroundStyle(.black)
                }
                Image(systemName: "chevron.down").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            }
            .foregroundStyle(Palette.text)
        }
        .accessibilityIdentifier("machineMenu")
        .accessibilityLabel("Machine: \(model.current?.name ?? "none")")
    }

    private func title(_ server: PairedServer) -> String {
        var parts = [server.name]
        switch model.connections[server.id]?.state {
        case .online?: break
        case .connecting?: parts.append("connecting")
        case .revoked?: parts.append("revoked")
        default: parts.append("offline")
        }
        let waiting = model.needsYou(server.id)
        if waiting > 0 { parts.append("\(waiting) need\(waiting == 1 ? "s" : "") you") }
        let reports = model.unreadReports(server.id)
        if reports > 0 { parts.append("\(reports) new report\(reports == 1 ? "" : "s")") }
        return parts.joined(separator: " · ")
    }

    private func symbol(_ server: PairedServer) -> String {
        model.needsYou(server.id) > 0 ? "exclamationmark.bubble" : "desktopcomputer"
    }

    private func stateColor(_ state: ConnState?) -> Color {
        switch state {
        case .online?: return .clear
        case .connecting?: return Palette.muted
        case .revoked?: return Palette.failed
        default: return Palette.attention
        }
    }
}

/// Sessions of the other machines that need you or finished a turn; a tap
/// switches there and opens the session.
struct AttentionStrip: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        let items = model.attention
        if !items.isEmpty {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(items) { item in
                        Button {
                            model.open(session: item.session.id, on: item.server.id)
                        } label: {
                            HStack(spacing: 6) {
                                Circle().fill(Palette.group(UInt8(item.server.colorIndex))).frame(width: 7, height: 7)
                                Text(item.server.name).font(.caption.weight(.semibold)).foregroundStyle(Palette.muted)
                                Image(systemName: item.session.status.symbol).font(.caption).foregroundStyle(item.session.status.color)
                                Text(item.session.name).font(.caption.weight(.medium)).foregroundStyle(Palette.text).lineLimit(1)
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 6)
                            .background(item.session.status == .needsYou ? Palette.attentionBg : Palette.finishedBg, in: Capsule())
                            .overlay(Capsule().stroke(item.session.status.color.opacity(0.6), lineWidth: 1))
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("attention-chip")
                        .accessibilityLabel("\(item.server.name): \(item.session.name), \(item.session.statusLabel)")
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
            }
            .background(Palette.panel)
            .overlay(alignment: .bottom) { Rectangle().fill(Palette.border).frame(height: 0.5) }
        }
    }
}

/// Naming and colouring a machine (after pairing, or from Settings).
struct MachineSheet: View {
    let server: PairedServer
    var done: (String, Int) -> Void
    @State private var name: String
    @State private var color: Int
    @FocusState private var focused: Bool

    init(server: PairedServer, done: @escaping (String, Int) -> Void) {
        self.server = server
        self.done = done
        _name = State(initialValue: server.name)
        _color = State(initialValue: server.colorIndex)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Name", text: $name)
                        .focused($focused)
                        .submitLabel(.done)
                        .onSubmit { done(name, color) }
                        .accessibilityIdentifier("machineName")
                } header: {
                    Text("Name")
                } footer: {
                    Text("For example work or personal. Shown in the machine switcher and in alerts.")
                }
                Section("Colour") {
                    HStack(spacing: 14) {
                        ForEach(Palette.groups.indices, id: \.self) { index in
                            Button {
                                color = index
                            } label: {
                                Circle()
                                    .fill(Palette.groups[index])
                                    .frame(width: 30, height: 30)
                                    .overlay(Circle().stroke(Color.white, lineWidth: color == index ? 3 : 0).padding(-4))
                            }
                            .buttonStyle(.plain)
                            .accessibilityLabel("Colour \(index + 1)")
                        }
                    }
                    .padding(.vertical, 6)
                }
                Section {
                    LabeledContent("Address", value: server.url).font(.footnote)
                    LabeledContent("This phone there", value: server.clientId).font(.footnote)
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.background)
            .scrollDismissesKeyboard(.interactively)
            .keyboardDoneButton()
            .navigationTitle("Machine")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { done(name, color) }.disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
        .presentationDetents([.medium, .large])
    }
}
