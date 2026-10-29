import SwiftUI

/// Paired services, the lock, the terminal's text size, and how this works.
struct SettingsView: View {
    @ObservedObject var connection: ServiceConnection
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @AppStorage("terminalFontSize") private var fontSize: Double = 12.5
    @AppStorage("terminalMirror") private var mirror = false
    @State private var pairing = false
    @State private var forgetting: PairedServer?

    var body: some View {
        NavigationStack {
            Form {
                Section("Services") {
                    ForEach(model.servers) { server in
                        Button {
                            model.select(server.id)
                        } label: {
                            HStack {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(server.name).foregroundStyle(Palette.text)
                                    Text(server.url).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                                    if !server.token.isEmpty {
                                        Text(tokenLine(server)).font(.caption2).foregroundStyle(server.tokenExpired ? Palette.failed : .secondary)
                                    }
                                }
                                Spacer()
                                if server.id == model.current?.id {
                                    Image(systemName: "checkmark").foregroundStyle(Palette.accent)
                                }
                            }
                        }
                        .swipeActions {
                            Button(role: .destructive) { forgetting = server } label: { Label("Forget", systemImage: "trash") }
                        }
                    }
                    Button { pairing = true } label: { Label("Pair another service", systemImage: "plus") }
                }
                Section {
                    LabeledContent("Connection", value: stateText)
                    LabeledContent("This phone", value: model.current?.clientId ?? "")
                } footer: {
                    Text("Revoke this phone on the Cloud PC with `copilot-canvas remote revoke \(model.current?.clientId ?? "ID")`.")
                }
                Section("Terminal") {
                    Stepper(value: $fontSize, in: 7...28, step: 0.5) {
                        LabeledContent("Text size", value: String(format: "%.1f pt", fontSize))
                    }
                    Toggle("Mirror the desktop's size", isOn: $mirror)
                } 
                Section {
                    Toggle("Lock with Face ID", isOn: Binding(get: { model.lockEnabled }, set: { model.lockEnabled = $0 }))
                } footer: {
                    Text("A paired phone can do what you can on the Cloud PC: keep it locked.")
                }
                Section("How sizes work") {
                    Text("A terminal follows the device that typed in it last (like tmux): typing here fits it to the phone; typing at the desk gives it back. Mirror mode shows the desktop's size without changing it.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
                Section {
                    LabeledContent("App", value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
                    LabeledContent("Core", value: coreVersion())
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.background)
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .sheet(isPresented: $pairing) {
                NavigationStack {
                    PairView { server in
                        model.add(server)
                        pairing = false
                    }
                    .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { pairing = false } } }
                }
            }
            .confirmationDialog("Forget \(forgetting?.name ?? "")?", isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } }), titleVisibility: .visible) {
                Button("Forget", role: .destructive) {
                    if let server = forgetting { model.forget(server.id) }
                    forgetting = nil
                }
            } message: {
                Text("This removes the pairing from this phone. Revoke it on the Cloud PC too.")
            }
        }
    }

    private var stateText: String {
        switch connection.state {
        case .online: return "Online"
        case .connecting: return "Connecting"
        case .offline: return "Offline"
        case .revoked: return "Revoked"
        }
    }

    private func tokenLine(_ server: PairedServer) -> String {
        guard server.tokenExpiresMs > 0 else { return "Tunnel access token" }
        let date = Date(timeIntervalSince1970: Double(server.tokenExpiresMs) / 1000)
        let when = date.formatted(date: .abbreviated, time: .shortened)
        return server.tokenExpired ? "Tunnel token expired \(when)" : "Tunnel token valid until \(when)"
    }
}
