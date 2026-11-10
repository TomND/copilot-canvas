import SwiftUI

/// A file path tapped in a terminal (`C:\src\app.rs:42`): the path on the
/// service's machine and the line to show.
struct FileTarget: Identifiable, Equatable {
    let id = UUID()
    let path: String
    let line: Int?

    init(path raw: String) {
        // `path:line` or `path:line:column` (not the drive's colon).
        var parts = raw.components(separatedBy: ":")
        var numbers: [Int] = []
        while parts.count > 1, let last = parts.last, let number = Int(last) {
            numbers.insert(number, at: 0)
            parts.removeLast()
        }
        path = parts.joined(separator: ":")
        line = numbers.first
    }
}

/// A read-only view of a text file on the service's machine (the first
/// 512 KB), with line numbers; nothing is ever run.
struct FileViewer: View {
    @ObservedObject var connection: ServiceConnection
    let target: FileTarget
    @Environment(\.dismiss) private var dismiss
    @State private var lines: [String] = []
    @State private var error: String?
    @State private var loading = true

    var body: some View {
        NavigationStack {
            Group {
                if loading {
                    ProgressView("Reading \(name)…")
                } else if let error {
                    ContentUnavailableView("Cannot show this file", systemImage: "doc.questionmark", description: Text(error))
                } else {
                    content
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(rgb: TerminalColors.background))
            .navigationTitle(name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .topBarLeading) {
                    Menu {
                        Button {
                            UIPasteboard.general.string = target.path
                        } label: { Label("Copy path", systemImage: "doc.on.doc") }
                        if !lines.isEmpty {
                            Button {
                                UIPasteboard.general.string = lines.joined(separator: "\n")
                            } label: { Label("Copy text", systemImage: "doc.on.clipboard") }
                            ShareLink(item: lines.joined(separator: "\n"), subject: Text(name)) { Label("Share text", systemImage: "square.and.arrow.up") }
                        }
                    } label: { Image(systemName: "ellipsis.circle") }
                }
            }
            .task { await load() }
        }
    }

    private var name: String {
        target.path.components(separatedBy: CharacterSet(charactersIn: "/\\")).last(where: { !$0.isEmpty }) ?? target.path
    }

    private var content: some View {
        ScrollViewReader { proxy in
            ScrollView([.vertical, .horizontal]) {
                LazyVStack(alignment: .leading, spacing: 0) {
                    Text(target.path).font(.caption2).foregroundStyle(.secondary).padding(.bottom, 8)
                    ForEach(lines.indices, id: \.self) { index in
                        HStack(alignment: .top, spacing: 10) {
                            Text("\(index + 1)")
                                .frame(minWidth: 38, alignment: .trailing)
                                .foregroundStyle(Palette.muted)
                            Text(lines[index].isEmpty ? " " : lines[index])
                                .foregroundStyle(Palette.text)
                                .fixedSize(horizontal: true, vertical: false)
                        }
                        .font(.system(size: 12, design: .monospaced))
                        .padding(.vertical, 1)
                        .background(index + 1 == target.line ? Palette.accent.opacity(0.18) : Color.clear)
                        .id(index + 1)
                    }
                }
                .padding(12)
                .textSelection(.enabled)
            }
            .onAppear {
                if let line = target.line { proxy.scrollTo(line, anchor: .center) }
            }
        }
    }

    private func load() async {
        let path = target.path
        do {
            let text = try await connection.perform { try $0.readText(path: path) }
            // Tabs as spaces, other control characters shown as dots: nothing is interpreted.
            lines = text.replacingOccurrences(of: "\r\n", with: "\n").components(separatedBy: "\n").map { line in
                String(line.replacingOccurrences(of: "\t", with: "    ").map { $0.isASCII && $0.asciiValue.map { $0 < 32 } == true ? "·" : $0 })
            }
        } catch {
            self.error = describe(error)
        }
        loading = false
    }
}
