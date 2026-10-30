import SwiftUI
import UIKit

/// Closing the keyboard, the same way everywhere: a tap outside any text
/// input, a Done button above the keyboard, a swipe down on forms and lists.
enum Keyboard {
    static func dismiss() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
    }
}

/// A tap anywhere that is not a text input (or the terminal, whose taps go to
/// the agent) closes the keyboard. Installed once on the app's window, so it
/// also covers sheets; buttons still get their tap.
final class TapOutsideDismisser: NSObject, UIGestureRecognizerDelegate {
    static let shared = TapOutsideDismisser()
    private static let name = "copilot-canvas.dismiss-keyboard"

    func install(on window: UIWindow) {
        guard !(window.gestureRecognizers ?? []).contains(where: { $0.name == Self.name }) else { return }
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        tap.name = Self.name
        tap.cancelsTouchesInView = false
        tap.delaysTouchesBegan = false
        tap.delaysTouchesEnded = false
        tap.delegate = self
        window.addGestureRecognizer(tap)
    }

    @objc private func tapped() {
        Keyboard.dismiss()
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        var view = touch.view
        while let current = view {
            if current is UITextField || current is UITextView || current is UISearchBar || current is TerminalView {
                return false
            }
            view = current.superview
        }
        return true
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        true
    }
}

/// Installs [`TapOutsideDismisser`] on the window this view ends up in.
struct KeyboardDismissInstaller: UIViewRepresentable {
    func makeUIView(context: Context) -> InstallerView { InstallerView() }
    func updateUIView(_ view: InstallerView, context: Context) {}

    final class InstallerView: UIView {
        override func didMoveToWindow() {
            super.didMoveToWindow()
            if let window { TapOutsideDismisser.shared.install(on: window) }
            isUserInteractionEnabled = false
        }
    }
}

extension View {
    /// A "hide keyboard" button above the keyboard of this view's text fields.
    func keyboardDoneButton() -> some View {
        toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button {
                    Keyboard.dismiss()
                } label: {
                    Image(systemName: "keyboard.chevron.compact.down")
                }
                .accessibilityLabel("Hide keyboard")
            }
        }
    }
}
