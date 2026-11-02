import SwiftUI
import UIKit

/// A note's source in a plain-text editor: monospaced, no smart quotes or
/// autocorrection, a key row for common Markdown, undo and redo. Save
/// writes on the revision it opened: a note changed elsewhere meanwhile is
/// never overwritten unasked (*Reload* drops your edits after asking, *Keep
/// mine* overwrites after asking again, as the desktop's editor does).
/// Closing with unsaved changes asks first.
struct NoteEditor: View {
    @ObservedObject var connection: ServiceConnection
    let path: String
    /// A note just made: the keyboard comes up with the caret at its end.
    var newNote = false
    /// Called after a save.
    var saved: () -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var source: NoteSource?
    @State private var text = ""
    @State private var original = ""
    @State private var base = ""
    @State private var error: String?
    @State private var saving = false
    /// The note as it is now, after a save found it changed (text, revision).
    @State private var conflict: (text: String, rev: String)?
    @State private var askReload = false
    @State private var askOverwrite = false
    @State private var askDiscard = false
    /// Bumped when the text is replaced from outside (a reload): the editor takes it.
    @State private var generation = 0

    private var dirty: Bool { source != nil && text != original }
    private var name: String { path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? path }

    var body: some View {
        NavigationStack {
            content
                .background(Palette.background)
                .navigationTitle(name)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") {
                            if dirty { askDiscard = true } else { dismiss() }
                        }
                        .accessibilityIdentifier("cancelEdit")
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        if saving {
                            ProgressView()
                        } else {
                            Button("Save") { save(base: base) }
                                .fontWeight(.semibold)
                                .disabled(!dirty || source?.editable != true || connection.state != .online)
                                .accessibilityIdentifier("saveNote")
                        }
                    }
                }
        }
        .interactiveDismissDisabled(dirty)
        .task { await load() }
        .confirmationDialog("Discard your changes?", isPresented: $askDiscard, titleVisibility: .visible) {
            Button("Discard changes", role: .destructive) { dismiss() }
            Button("Keep editing", role: .cancel) {}
        } message: {
            Text("Your edits to \(name) were not saved.")
        }
        .alert("This note changed elsewhere", isPresented: Binding(get: { conflict != nil && !askReload && !askOverwrite }, set: { if !$0 && !askReload && !askOverwrite { conflict = nil } })) {
            Button("Reload theirs…") { askReload = true }
            Button("Keep mine…") { askOverwrite = true }
            Button("Keep editing", role: .cancel) { conflict = nil }
        } message: {
            Text("Since you opened it, \(name) was changed on \(connection.server.name) (by an agent, a window or another phone). Nothing was written.")
        }
        .confirmationDialog("Load the note as it is now?", isPresented: $askReload, titleVisibility: .visible) {
            Button("Discard my edits and reload", role: .destructive) {
                if let conflict { take(text: conflict.text, rev: conflict.rev) }
                conflict = nil
            }
            Button("Cancel", role: .cancel) { conflict = nil }
        } message: {
            Text("Your edits are lost.")
        }
        .confirmationDialog("Overwrite the other change?", isPresented: $askOverwrite, titleVisibility: .visible) {
            Button("Overwrite with mine", role: .destructive) {
                if let conflict { save(base: conflict.rev) }
                conflict = nil
            }
            Button("Cancel", role: .cancel) { conflict = nil }
        } message: {
            Text("What was changed elsewhere is replaced by your text.")
        }
    }

    @ViewBuilder
    private var content: some View {
        if let source {
            if source.editable {
                VStack(spacing: 0) {
                    if let error {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(.footnote)
                            .foregroundStyle(Palette.failed)
                            .padding(8)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(Palette.panel)
                    }
                    SourceTextView(text: $text, generation: generation, markdown: source.kind == .markdown, focusAtEnd: newNote)
                }
            } else {
                ContentUnavailableView("This file cannot be edited here", systemImage: "pencil.slash", description: Text(source.whyNot))
            }
        } else if let error {
            ContentUnavailableView("Cannot open the source", systemImage: "doc.questionmark", description: Text(error))
        } else {
            ProgressView("Loading the source…")
        }
    }

    private func load() async {
        let path = path
        do {
            let loaded = try await connection.perform { try $0.noteSource(path: path) }
            source = loaded
            take(text: loaded.text, rev: loaded.rev)
        } catch {
            self.error = describe(error)
        }
    }

    /// The editor shows `text` at revision `rev` (unchanged).
    private func take(text: String, rev: String) {
        self.text = text
        original = text
        base = rev
        generation += 1
    }

    private func save(base: String) {
        let path = path, mine = text
        saving = true
        error = nil
        Task {
            do {
                let result = try await connection.perform { try $0.saveNote(path: path, text: mine, base: base) }
                saving = false
                original = result.text
                text = result.text
                self.base = result.rev
                saved()
                dismiss()
            } catch let CoreError.Conflict(_, now, rev) {
                saving = false
                conflict = (now, rev)
            } catch {
                saving = false
                self.error = describe(error)
            }
        }
    }
}

/// The source text: a `UITextView` (its own undo, Dynamic Type) with a key
/// row above the keyboard.
struct SourceTextView: UIViewRepresentable {
    @Binding var text: String
    /// Changes when `text` was replaced from outside.
    let generation: Int
    let markdown: Bool
    /// Take the keyboard with the caret at the end once the text is in (a new note).
    var focusAtEnd = false

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.text = text
        view.font = UIFontMetrics(forTextStyle: .body).scaledFont(for: .monospacedSystemFont(ofSize: 15, weight: .regular))
        view.adjustsFontForContentSizeCategory = true
        view.textColor = TerminalColors.ui(0xC9D1D9)
        view.backgroundColor = TerminalColors.ui(TerminalColors.background)
        view.tintColor = TerminalColors.ui(0x58A6FF)
        view.autocorrectionType = .no
        view.autocapitalizationType = .none
        view.spellCheckingType = .no
        view.smartQuotesType = .no
        view.smartDashesType = .no
        view.smartInsertDeleteType = .no
        view.keyboardDismissMode = .interactive
        view.alwaysBounceVertical = true
        view.textContainerInset = UIEdgeInsets(top: 12, left: 10, bottom: 24, right: 10)
        view.delegate = context.coordinator
        view.accessibilityIdentifier = "noteSource"
        view.accessibilityLabel = "Note source"
        view.inputAccessoryView = context.coordinator.keyRow(for: view, markdown: markdown)
        context.coordinator.generation = generation
        if focusAtEnd {
            context.coordinator.focused = true
            context.coordinator.focusAtEnd(view)
        }
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.parent = self
        // Only a reload replaces the text (setting it would clear the undo history).
        if context.coordinator.generation != generation {
            context.coordinator.generation = generation
            view.text = text
            if focusAtEnd && !context.coordinator.focused {
                context.coordinator.focused = true
                context.coordinator.focusAtEnd(view)
            }
        }
    }

    @MainActor
    final class Coordinator: NSObject, UITextViewDelegate {
        var parent: SourceTextView
        var generation = 0
        var focused = false
        weak var view: UITextView?

        init(_ parent: SourceTextView) {
            self.parent = parent
        }

        /// The keyboard, the caret at the end, once the editor is on screen
        /// (it is still being presented when its text arrives).
        func focusAtEnd(_ view: UITextView, attempts: Int = 30) {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { [weak self, weak view] in
                guard let view else { return }
                if view.window == nil || !view.becomeFirstResponder() {
                    if attempts > 0 { self?.focusAtEnd(view, attempts: attempts - 1) }
                    return
                }
                view.selectedRange = NSRange(location: (view.text as NSString).length, length: 0)
                view.scrollRangeToVisible(view.selectedRange)
            }
        }

        func textViewDidChange(_ textView: UITextView) {
            parent.text = textView.text
        }

        /// What a key of the row does.
        enum Edit {
            /// Puts `prefix` at the start of the line.
            case line(String)
            /// Wraps the selection (or puts the caret between).
            case wrap(String, String)
            case undo, redo, hide
        }

        private struct Key {
            let title: String
            let label: String
            let edit: Edit
        }

        func keyRow(for view: UITextView, markdown: Bool) -> UIView {
            self.view = view
            let keys: [Key] = markdown ? [
                Key(title: "#", label: "Heading", edit: .line("# ")),
                Key(title: "B", label: "Bold", edit: .wrap("**", "**")),
                Key(title: "I", label: "Italic", edit: .wrap("_", "_")),
                Key(title: "`", label: "Code", edit: .wrap("`", "`")),
                Key(title: "•", label: "List item", edit: .line("- ")),
                Key(title: "[ ]", label: "Task", edit: .line("- [ ] ")),
                Key(title: ">", label: "Quote", edit: .line("> ")),
                Key(title: "[[ ]]", label: "Note link", edit: .wrap("[[", "]]")),
                Key(title: "[](…)", label: "Link", edit: .wrap("[", "](https://)")),
            ] : []
            let bar = UIInputView(frame: CGRect(x: 0, y: 0, width: 320, height: 46), inputViewStyle: .keyboard)
            bar.allowsSelfSizing = true
            let scroll = UIScrollView()
            scroll.showsHorizontalScrollIndicator = false
            let row = UIStackView()
            row.axis = .horizontal
            row.spacing = 6
            row.isLayoutMarginsRelativeArrangement = true
            row.layoutMargins = UIEdgeInsets(top: 6, left: 8, bottom: 6, right: 8)
            for key in keys { row.addArrangedSubview(button(key.title, label: key.label, edit: key.edit)) }
            row.addArrangedSubview(button(symbol: "arrow.uturn.forward", label: "Redo", edit: .redo))
            scroll.addSubview(row)
            let fixed = UIStackView(arrangedSubviews: [
                button(symbol: "arrow.uturn.backward", label: "Undo", edit: .undo),
                button(symbol: "keyboard.chevron.compact.down", label: "Hide keyboard", edit: .hide),
            ])
            fixed.spacing = 6
            fixed.isLayoutMarginsRelativeArrangement = true
            fixed.layoutMargins = UIEdgeInsets(top: 6, left: 4, bottom: 6, right: 8)
            for view in [scroll, row, fixed] as [UIView] { view.translatesAutoresizingMaskIntoConstraints = false }
            bar.addSubview(scroll)
            bar.addSubview(fixed)
            NSLayoutConstraint.activate([
                bar.heightAnchor.constraint(equalToConstant: 46),
                scroll.leadingAnchor.constraint(equalTo: bar.leadingAnchor),
                scroll.topAnchor.constraint(equalTo: bar.topAnchor),
                scroll.bottomAnchor.constraint(equalTo: bar.bottomAnchor),
                scroll.trailingAnchor.constraint(equalTo: fixed.leadingAnchor),
                fixed.trailingAnchor.constraint(equalTo: bar.trailingAnchor),
                fixed.topAnchor.constraint(equalTo: bar.topAnchor),
                fixed.bottomAnchor.constraint(equalTo: bar.bottomAnchor),
                row.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
                row.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
                row.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
                row.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
                row.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
            ])
            return bar
        }

        private func button(_ title: String = "", symbol: String? = nil, label: String, edit: Edit) -> UIButton {
            var config = UIButton.Configuration.gray()
            config.cornerStyle = .medium
            config.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 9, bottom: 4, trailing: 9)
            if let symbol {
                config.image = UIImage(systemName: symbol)
            } else {
                var attributed = AttributedString(title)
                attributed.font = .monospacedSystemFont(ofSize: 15, weight: .semibold)
                config.attributedTitle = attributed
            }
            let button = UIButton(configuration: config, primaryAction: UIAction { [weak self] _ in self?.apply(edit) })
            button.accessibilityLabel = label
            return button
        }

        private func apply(_ edit: Edit) {
            guard let view else { return }
            switch edit {
            case .undo:
                view.undoManager?.undo()
            case .redo:
                view.undoManager?.redo()
            case .hide:
                view.resignFirstResponder()
            case .line(let prefix):
                guard let range = view.selectedTextRange else { return }
                let text = view.text as NSString
                let caret = view.offset(from: view.beginningOfDocument, to: range.start)
                let lineStart = text.lineRange(for: NSRange(location: min(caret, text.length), length: 0)).location
                if let start = view.position(from: view.beginningOfDocument, offset: lineStart), let at = view.textRange(from: start, to: start) {
                    view.replace(at, withText: prefix)
                }
            case .wrap(let open, let close):
                guard let range = view.selectedTextRange else { return }
                let selected = view.text(in: range) ?? ""
                view.replace(range, withText: open + selected + close)
                // The caret between the marks (or after the wrapped text).
                if let end = view.selectedTextRange?.end, let caret = view.position(from: end, offset: -close.count) {
                    view.selectedTextRange = view.textRange(from: caret, to: caret)
                }
            }
            parent.text = view.text
        }
    }
}
