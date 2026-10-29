import UIKit

/// The row above the keyboard: the keys a phone keyboard lacks (Esc, Tab,
/// Ctrl, arrows, …), sticky Ctrl and Alt, paste and the prompt composer.
final class KeyBar: UIInputView {
    private weak var terminal: TerminalView?
    private var ctrlButton: UIButton?
    private var altButton: UIButton?
    /// Opens the prompt composer (set by the screen).
    var onCompose: (() -> Void)?

    private enum Item {
        case key(String, KeyCode, UInt8)
        case sticky(String, UInt8)
        case action(String, String, Selector)
    }

    init(terminal: TerminalView) {
        self.terminal = terminal
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: 46), inputViewStyle: .keyboard)
        allowsSelfSizing = true
        // [ keys, scrolling sideways | hide keyboard, always visible ]
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.alwaysBounceVertical = false
        scroll.isDirectionalLockEnabled = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        addSubview(scroll)
        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        let hide = button(.action("keyboard.chevron.compact.down", "Hide keyboard", #selector(hideKeyboard)))
        hide.configuration?.baseBackgroundColor = UIColor(white: 0.45, alpha: 1)
        hide.translatesAutoresizingMaskIntoConstraints = false
        addSubview(hide)
        // A thin edge so the keys visibly slide under the pinned button.
        let edge = UIView()
        edge.backgroundColor = UIColor(white: 1, alpha: 0.15)
        edge.translatesAutoresizingMaskIntoConstraints = false
        addSubview(edge)
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 46),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            scroll.trailingAnchor.constraint(equalTo: edge.leadingAnchor, constant: -4),
            edge.widthAnchor.constraint(equalToConstant: 1),
            edge.topAnchor.constraint(equalTo: topAnchor, constant: 10),
            edge.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -10),
            edge.trailingAnchor.constraint(equalTo: hide.leadingAnchor, constant: -6),
            hide.trailingAnchor.constraint(equalTo: safeAreaLayoutGuide.trailingAnchor, constant: -6),
            hide.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            hide.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -2),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 6),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -6),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor, constant: -12),
        ])
        let items: [Item] = [
            .action("square.and.pencil", "Compose", #selector(compose)),
            .key("esc", .esc, 0),
            .sticky("ctrl", 4),
            .sticky("alt", 2),
            .key("tab", .tab, 0),
            .key("⇧tab", .backTab, 0),
            .key("↑", .up, 0),
            .key("↓", .down, 0),
            .key("←", .left, 0),
            .key("→", .right, 0),
            .key("^C", .char(text: "c"), 4),
            .key("^D", .char(text: "d"), 4),
            .key("/", .char(text: "/"), 0),
            .key("|", .char(text: "|"), 0),
            .key("~", .char(text: "~"), 0),
            .key("-", .char(text: "-"), 0),
            .key("home", .home, 0),
            .key("end", .end, 0),
            .key("pgup", .pageUp, 0),
            .key("pgdn", .pageDown, 0),
            .action("doc.on.clipboard", "Paste", #selector(pasteClipboard)),
            .action("photo", "Send a picture", #selector(photo)),
        ]
        for item in items {
            stack.addArrangedSubview(button(item))
        }
        // A swipe down on the bar closes the keyboard (sideways still scrolls the keys).
        let swipe = UISwipeGestureRecognizer(target: self, action: #selector(hideKeyboard))
        swipe.direction = .down
        swipe.delegate = self
        addGestureRecognizer(swipe)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func button(_ item: Item) -> UIButton {
        var config = UIButton.Configuration.filled()
        config.baseBackgroundColor = UIColor(white: 0.32, alpha: 1)
        config.baseForegroundColor = .white
        config.cornerStyle = .medium
        config.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 10, bottom: 4, trailing: 10)
        let button = UIButton(configuration: config)
        switch item {
        case .key(let title, let key, let mods):
            button.configuration?.title = title
            button.addAction(UIAction { [weak self] _ in
                guard let terminal = self?.terminal else { return }
                UIDevice.current.playInputClick()
                terminal.onKey?(key, mods | terminal.stickyMods)
                terminal.stickyMods = 0
            }, for: .touchUpInside)
        case .sticky(let title, let bit):
            button.configuration?.title = title
            button.addAction(UIAction { [weak self] _ in
                guard let terminal = self?.terminal else { return }
                terminal.stickyMods ^= bit
            }, for: .touchUpInside)
            if bit == 4 { ctrlButton = button } else { altButton = button }
        case .action(let symbol, let label, let selector):
            button.configuration?.image = UIImage(systemName: symbol)
            button.accessibilityLabel = label
            button.addTarget(self, action: selector, for: .touchUpInside)
            button.widthAnchor.constraint(equalToConstant: 46).isActive = true
        }
        button.setContentHuggingPriority(.required, for: .horizontal)
        button.setContentCompressionResistancePriority(.required, for: .horizontal)
        button.configuration?.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
            var attributes = attributes
            attributes.font = UIFont.monospacedSystemFont(ofSize: 14, weight: .medium)
            return attributes
        }
        return button
    }

    func updateSticky(_ mods: UInt8) {
        ctrlButton?.configuration?.baseBackgroundColor = mods & 4 != 0 ? .systemBlue : UIColor(white: 0.32, alpha: 1)
        altButton?.configuration?.baseBackgroundColor = mods & 2 != 0 ? .systemBlue : UIColor(white: 0.32, alpha: 1)
    }

    @objc private func compose() { onCompose?() }

    @objc private func pasteClipboard() {
        terminal?.paste(nil)
    }

    @objc private func photo() {
        terminal?.onPhoto?()
    }

    @objc private func hideKeyboard() { terminal?.resignFirstResponder() }
}

extension KeyBar: UIInputViewAudioFeedback {
    var enableInputClicksWhenVisible: Bool { true }
}

extension KeyBar: UIGestureRecognizerDelegate {
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        true
    }
}
