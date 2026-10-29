import CoreText
import UIKit

/// Cell-grid terminal drawn with Core Text: the screens the service's
/// terminal engine (libghostty-vt on the Cloud PC) produces, as grapheme
/// cells with colours, attributes, every underline style, cursor shapes and
/// box-drawing characters drawn as lines (so borders join, as in Ghostty).
/// Typing goes to the service as keys (it encodes them for the program).
final class TerminalView: UIView, UIKeyInput, UIGestureRecognizerDelegate {
    // MARK: Model (what the service sent)

    private(set) var cols: Int = 0
    private(set) var rows: Int = 0
    private var lines: [[ScreenRun]] = []
    private var userRows: [Bool] = []
    private var cursor: (visible: Bool, col: Int, row: Int, shape: UInt8) = (false, 0, 0, 0)
    private(set) var modes: UInt32 = 0
    private var links: [String] = []
    private var displayOffset: UInt32 = 0

    // MARK: Appearance

    /// Font size chosen by the user (pinch); mirror mode may draw smaller.
    var fontSize: CGFloat = 13 {
        didSet { if fontSize != oldValue { fontsChanged() } }
    }
    /// Draw the service's grid scaled to fit (mirror mode) instead of sizing the grid to the view.
    var mirror = false {
        didSet { if mirror != oldValue { fontsChanged() } }
    }
    private let padding: CGFloat = 4
    private var fonts: Fonts!
    private var drawnSize: CGFloat = 0

    // MARK: Callbacks

    var onKey: ((KeyCode, UInt8) -> Void)?
    var onText: ((String) -> Void)?
    var onPaste: ((String) -> Void)?
    var onWheel: ((Bool, UInt16, UInt16) -> Void)?
    var onClick: ((UInt16, UInt16) -> Void)?
    /// The grid that fits the view at the user's font size (fit mode).
    var onGridSize: ((Int, Int) -> Void)?
    var onFontSize: ((CGFloat) -> Void)?
    /// Selected cells (inclusive) → the text, from the core.
    var selectionText: ((UInt16, UInt16, UInt16, UInt16) -> String)?
    var onCopied: (() -> Void)?
    var onLink: ((String) -> Void)?

    /// Sticky modifiers from the key bar (apply to the next key).
    var stickyMods: UInt8 = 0 {
        didSet { keyBar.updateSticky(stickyMods) }
    }

    private lazy var keyBar = KeyBar(terminal: self)
    /// The key bar's compose button.
    var onCompose: (() -> Void)? {
        get { keyBar.onCompose }
        set { keyBar.onCompose = newValue }
    }
    private var selection: (anchor: (Int, Int), head: (Int, Int))?
    private var wheelRemainder: CGFloat = 0
    private var pinchStart: CGFloat = 13
    private var lastGrid = (0, 0)

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = TerminalColors.ui(TerminalColors.background)
        isOpaque = true
        contentMode = .redraw
        fonts = Fonts(size: fontSize)
        drawnSize = fontSize
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        pan.maximumNumberOfTouches = 1
        let press = UILongPressGestureRecognizer(target: self, action: #selector(pressed(_:)))
        press.minimumPressDuration = 0.35
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        for recognizer in [tap, pan, press, pinch] as [UIGestureRecognizer] {
            recognizer.delegate = self
            addGestureRecognizer(recognizer)
        }
        isAccessibilityElement = true
        accessibilityTraits = .allowsDirectInteraction
        accessibilityLabel = "Terminal"
        accessibilityIdentifier = "terminal"
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    // MARK: Screens

    func apply(_ screen: Screen) {
        let resized = Int(screen.cols) != cols || Int(screen.rows) != rows
        if screen.full || resized {
            cols = Int(screen.cols)
            rows = Int(screen.rows)
            lines = Array(repeating: [], count: rows)
            selection = nil
        }
        var dirty = IndexSet()
        for row in screen.changed where Int(row.y) < rows {
            lines[Int(row.y)] = row.runs
            dirty.insert(Int(row.y))
        }
        let user = screen.userRows.count == rows ? screen.userRows : Array(repeating: false, count: rows)
        for (y, marked) in user.enumerated() where y >= userRows.count || userRows[y] != marked {
            dirty.insert(y)
        }
        userRows = user
        let newCursor = (screen.cursorVisible, Int(screen.cursorCol), Int(screen.cursorRow), screen.cursorShape)
        if newCursor != cursor {
            dirty.insert(cursor.row)
            dirty.insert(newCursor.2)
            cursor = newCursor
        }
        modes = screen.modes
        links = screen.links
        displayOffset = screen.displayOffset
        if resized && mirror {
            fontsChanged()
        } else if screen.full || resized {
            setNeedsDisplay()
        } else {
            for y in dirty { setNeedsDisplay(rowRect(y)) }
        }
    }

    /// The visible text (VoiceOver, UI tests).
    override var accessibilityValue: String? {
        get { visibleText() }
        set {}
    }

    func visibleText() -> String {
        lines.map { runs in
            var line = ""
            var col = 0
            for run in runs {
                while col < Int(run.col) { line.append(" "); col += 1 }
                for cell in run.cells {
                    line.append(cell)
                    col += run.wide ? 2 : 1
                }
            }
            return line
        }
        .joined(separator: "\n")
    }

    // MARK: Geometry

    private var cellWidth: CGFloat { fonts.cellWidth }
    private var cellHeight: CGFloat { fonts.cellHeight }

    private func rowRect(_ row: Int) -> CGRect {
        CGRect(x: 0, y: CGFloat(row) * cellHeight, width: bounds.width, height: cellHeight + 1)
    }

    /// The cell under a point (clamped to the grid).
    func cell(at point: CGPoint) -> (Int, Int) {
        let col = Int((point.x - padding) / cellWidth)
        let row = Int(point.y / cellHeight)
        return (max(0, min(max(cols - 1, 0), col)), max(0, min(max(rows - 1, 0), row)))
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if mirror { updateMirrorFont() }
        reportGrid()
    }

    /// The grid that fits at the user's font size.
    private func reportGrid() {
        let sizing = Fonts(size: fontSize)
        let c = max(20, Int((bounds.width - 2 * padding) / sizing.cellWidth))
        let r = max(5, Int(bounds.height / sizing.cellHeight))
        guard bounds.width > 0, bounds.height > 0, (c, r) != lastGrid else { return }
        lastGrid = (c, r)
        onGridSize?(c, r)
    }

    private func fontsChanged() {
        if mirror {
            updateMirrorFont()
        } else {
            fonts = Fonts(size: fontSize)
            drawnSize = fontSize
        }
        lastGrid = (0, 0)
        reportGrid()
        setNeedsDisplay()
    }

    /// Mirror mode: the desktop's grid scaled down to fit the view.
    private func updateMirrorFont() {
        guard cols > 0 else { return }
        let base = Fonts(size: fontSize)
        let widthScale = (bounds.width - 2 * padding) / (CGFloat(cols) * base.cellWidth)
        let heightScale = bounds.height / (CGFloat(max(rows, 1)) * base.cellHeight)
        let size = max(4, min(fontSize, fontSize * min(widthScale, heightScale)))
        if abs(size - drawnSize) > 0.05 {
            fonts = Fonts(size: size)
            drawnSize = size
            setNeedsDisplay()
        }
    }

    // MARK: Drawing

    override func draw(_ rect: CGRect) {
        guard let ctx = UIGraphicsGetCurrentContext() else { return }
        ctx.setFillColor(TerminalColors.cg(TerminalColors.background))
        ctx.fill(rect)
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        let first = max(0, Int(rect.minY / cellHeight))
        let last = min(rows - 1, Int(rect.maxY / cellHeight))
        guard first <= last else { return }
        for row in first...last {
            drawRow(row, in: ctx)
        }
    }

    private func x(_ col: Int) -> CGFloat { padding + CGFloat(col) * cellWidth }

    private func drawRow(_ row: Int, in ctx: CGContext) {
        let top = CGFloat(row) * cellHeight
        let scale = window?.screen.scale ?? 2
        let snap = { (v: CGFloat) in (v * scale).rounded() / scale }
        // Your own prompts (Copilot): a navy background and a blue bar.
        if row < userRows.count, userRows[row] {
            ctx.setFillColor(TerminalColors.cg(TerminalColors.userBackground))
            ctx.fill(CGRect(x: 0, y: snap(top), width: bounds.width, height: snap(top + cellHeight) - snap(top)))
            ctx.setFillColor(TerminalColors.cg(TerminalColors.userBar))
            ctx.fill(CGRect(x: 0, y: snap(top), width: 2.5, height: snap(top + cellHeight) - snap(top)))
        }
        let runs = row < lines.count ? lines[row] : []
        // Backgrounds first, so wide glyphs and underlines are never covered.
        for run in runs {
            let (_, bg) = colors(run)
            guard bg != TerminalColors.background || run.flags & Flag.inverse != 0 else { continue }
            let width = run.wide ? 2 : 1
            let start = x(Int(run.col))
            let end = x(Int(run.col) + run.cells.count * width)
            ctx.setFillColor(TerminalColors.cg(bg))
            ctx.fill(CGRect(x: snap(start), y: snap(top), width: snap(end) - snap(start), height: snap(top + cellHeight) - snap(top)))
        }
        if let selection, let range = selectedColumns(row: row, selection: selection) {
            ctx.setFillColor(TerminalColors.cg(TerminalColors.selection, alpha: 0.85))
            ctx.fill(CGRect(x: x(range.lowerBound), y: top, width: CGFloat(range.count) * cellWidth, height: cellHeight))
        }
        let isCursorRow = cursor.visible && cursor.row == row && displayOffset == 0
        if isCursorRow, cursor.shape == 0 {
            ctx.setFillColor(TerminalColors.cg(TerminalColors.cursor))
            ctx.fill(CGRect(x: x(cursor.col), y: top, width: cellWidth, height: cellHeight))
        }
        for run in runs {
            drawText(run, row: row, top: top, in: ctx, cursorCol: isCursorRow && cursor.shape == 0 ? cursor.col : nil)
        }
        if isCursorRow, cursor.shape != 0 {
            ctx.setFillColor(TerminalColors.cg(TerminalColors.cursor))
            let cx = x(cursor.col)
            switch cursor.shape {
            case 1: ctx.fill(CGRect(x: cx, y: top + cellHeight - 2, width: cellWidth, height: 2))
            case 2: ctx.fill(CGRect(x: cx, y: top, width: 2, height: cellHeight))
            default:
                ctx.setStrokeColor(TerminalColors.cg(TerminalColors.cursor))
                ctx.setLineWidth(1)
                ctx.stroke(CGRect(x: cx + 0.5, y: top + 0.5, width: cellWidth - 1, height: cellHeight - 1))
            }
        }
    }

    /// Foreground and background of a run (inverse applied).
    private func colors(_ run: ScreenRun) -> (UInt32, UInt32) {
        var fg = TerminalColors.resolve(run.fg, default: TerminalColors.foreground)
        var bg = TerminalColors.resolve(run.bg, default: TerminalColors.background)
        if run.flags & Flag.inverse != 0 { swap(&fg, &bg) }
        return (fg, bg)
    }

    private func drawText(_ run: ScreenRun, row: Int, top: CGFloat, in ctx: CGContext, cursorCol: Int?) {
        if run.flags & Flag.hidden != 0 { return }
        let (fg, _) = colors(run)
        let alpha: CGFloat = run.flags & Flag.dim != 0 ? 0.6 : 1
        let font = fonts.font(bold: run.flags & Flag.bold != 0, italic: run.flags & Flag.italic != 0)
        let width = run.wide ? 2 : 1
        let baseline = top + fonts.baseline
        var col = Int(run.col)
        var batch = ""
        var batchStart = col
        func flush() {
            guard !batch.isEmpty else { return }
            drawString(batch, at: CGPoint(x: x(batchStart), y: baseline), font: font, color: TerminalColors.cg(fg, alpha: alpha), in: ctx)
            batch = ""
        }
        for cell in run.cells {
            let underCursor = cursorCol == col
            if let scalar = cell.unicodeScalars.first, cell.unicodeScalars.count == 1, BoxDrawing.handles(scalar) {
                flush()
                BoxDrawing.draw(scalar, in: CGRect(x: x(col), y: top, width: cellWidth * CGFloat(width), height: cellHeight), color: TerminalColors.cg(underCursor ? TerminalColors.background : fg, alpha: alpha), ctx: ctx)
            } else if underCursor {
                flush()
                drawString(cell, at: CGPoint(x: x(col), y: baseline), font: font, color: TerminalColors.cg(TerminalColors.background), in: ctx)
            } else if width == 1, cell.unicodeScalars.count == 1, let scalar = cell.unicodeScalars.first, scalar.isASCII {
                if batch.isEmpty { batchStart = col }
                batch.append(cell)
            } else {
                flush()
                // Wide and complex graphemes sit in their own cells.
                drawString(cell, at: CGPoint(x: x(col), y: baseline), font: font, color: TerminalColors.cg(fg, alpha: alpha), in: ctx, fit: cellWidth * CGFloat(width))
            }
            col += width
        }
        flush()
        decorate(run, top: top, fg: fg, alpha: alpha, in: ctx)
    }

    private func drawString(_ text: String, at point: CGPoint, font: CTFont, color: CGColor, in ctx: CGContext, fit: CGFloat? = nil) {
        let attributes: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
            NSAttributedString.Key(kCTLigatureAttributeName as String): 0,
        ]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attributes))
        var origin = point
        if let fit {
            // Center a fallback glyph (emoji, CJK) in its cells; shrink it if wider.
            let width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
            if width > fit + 0.5 {
                ctx.saveGState()
                let scale = fit / width
                ctx.translateBy(x: point.x, y: point.y)
                ctx.scaleBy(x: scale, y: scale)
                ctx.textPosition = .zero
                CTLineDraw(line, ctx)
                ctx.restoreGState()
                return
            }
            origin.x += (fit - width) / 2
        }
        ctx.textPosition = origin
        CTLineDraw(line, ctx)
    }

    /// Underline (single, double, curly, dotted, dashed), strike-through, overline.
    private func decorate(_ run: ScreenRun, top: CGFloat, fg: UInt32, alpha: CGFloat, in ctx: CGContext) {
        let underline = run.flags & Flag.underline != 0
        let strike = run.flags & Flag.strike != 0
        let overline = run.flags & Flag.overline != 0
        guard underline || strike || overline else { return }
        let start = x(Int(run.col))
        let end = x(Int(run.col) + run.cells.count * (run.wide ? 2 : 1))
        let thickness = max(1, fonts.size / 13)
        ctx.saveGState()
        defer { ctx.restoreGState() }
        if underline {
            let color = run.underline != 0 ? TerminalColors.resolve(run.underline, default: fg) : fg
            ctx.setStrokeColor(TerminalColors.cg(color, alpha: alpha))
            ctx.setLineWidth(thickness)
            let y = top + fonts.baseline + max(2, fonts.descent * 0.5)
            switch (run.flags >> 9) & 7 {
            case 2:
                line(ctx, start, end, y - thickness)
                line(ctx, start, end, y + thickness)
            case 3:
                let path = CGMutablePath()
                path.move(to: CGPoint(x: start, y: y))
                var px = start
                let step = max(2, cellWidth / 2)
                var up = true
                while px < end {
                    path.addQuadCurve(to: CGPoint(x: min(px + step, end), y: y), control: CGPoint(x: px + step / 2, y: y + (up ? -2 : 2) * thickness))
                    px += step
                    up.toggle()
                }
                ctx.addPath(path)
                ctx.strokePath()
            case 4:
                ctx.setLineDash(phase: 0, lengths: [thickness, thickness * 2])
                line(ctx, start, end, y)
            case 5:
                ctx.setLineDash(phase: 0, lengths: [cellWidth * 0.6, cellWidth * 0.4])
                line(ctx, start, end, y)
            default:
                line(ctx, start, end, y)
            }
            ctx.setLineDash(phase: 0, lengths: [])
        }
        ctx.setStrokeColor(TerminalColors.cg(fg, alpha: alpha))
        ctx.setLineWidth(thickness)
        if strike { line(ctx, start, end, top + cellHeight / 2) }
        if overline { line(ctx, start, end, top + thickness / 2) }
    }

    private func line(_ ctx: CGContext, _ from: CGFloat, _ to: CGFloat, _ y: CGFloat) {
        ctx.move(to: CGPoint(x: from, y: y))
        ctx.addLine(to: CGPoint(x: to, y: y))
        ctx.strokePath()
    }

    // MARK: Selection

    private func selectedColumns(row: Int, selection: (anchor: (Int, Int), head: (Int, Int))) -> Range<Int>? {
        var (a, b) = (selection.anchor, selection.head)
        if (b.1, b.0) < (a.1, a.0) { swap(&a, &b) }
        guard row >= a.1, row <= b.1 else { return nil }
        let from = row == a.1 ? a.0 : 0
        let to = row == b.1 ? b.0 : cols - 1
        return from <= to ? from..<(to + 1) : nil
    }

    // MARK: Gestures

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        g is UIPinchGestureRecognizer || other is UIPinchGestureRecognizer
    }

    @objc private func tapped(_ g: UITapGestureRecognizer) {
        let (col, row) = cell(at: g.location(in: self))
        if selection != nil {
            selection = nil
            setNeedsDisplay()
        }
        if let link = link(at: col, row: row) {
            onLink?(link)
            return
        }
        if !isFirstResponder { becomeFirstResponder() }
        // Programs that read the mouse (Copilot's tab bar, "click here" actions) get the tap.
        if modes & 1 != 0 { onClick?(UInt16(col), UInt16(row)) }
    }

    /// An OSC 8 link, or a URL written in the row, under a cell.
    private func link(at col: Int, row: Int) -> String? {
        guard row < lines.count else { return nil }
        var c = 0
        var text = ""
        var hit: Int?
        for run in lines[row] {
            while c < Int(run.col) { text.append(" "); c += 1 }
            let width = run.wide ? 2 : 1
            for cell in run.cells {
                if c <= col && col < c + width {
                    if run.link > 0, Int(run.link) <= links.count { return links[Int(run.link) - 1] }
                    hit = text.count
                }
                text.append(cell)
                c += width
            }
        }
        guard let hit else { return nil }
        let characters = Array(text)
        let stops: Set<Character> = [" ", "\"", "'", "<", ">", "(", ")", "[", "]", "│", "┃"]
        var start = hit
        while start > 0 && !stops.contains(characters[start - 1]) { start -= 1 }
        var end = hit
        while end < characters.count && !stops.contains(characters[end]) { end += 1 }
        let word = String(characters[start..<end]).trimmingCharacters(in: CharacterSet(charactersIn: ".,;:"))
        return word.hasPrefix("http://") || word.hasPrefix("https://") ? word : nil
    }

    @objc private func panned(_ g: UIPanGestureRecognizer) {
        // One finger up or down: wheel notches (the program scrolls its own
        // view when it reads the mouse; else the scrollback moves).
        let dy = g.translation(in: self).y
        g.setTranslation(.zero, in: self)
        wheelRemainder += dy
        let notch = cellHeight * 1.5
        let (col, row) = cell(at: g.location(in: self))
        while abs(wheelRemainder) >= notch {
            let up = wheelRemainder > 0
            onWheel?(up, UInt16(col), UInt16(row))
            wheelRemainder -= up ? notch : -notch
        }
        if g.state == .ended || g.state == .cancelled { wheelRemainder = 0 }
    }

    @objc private func pressed(_ g: UILongPressGestureRecognizer) {
        let (col, row) = cell(at: g.location(in: self))
        switch g.state {
        case .began:
            selection = ((col, row), (col, row))
            UISelectionFeedbackGenerator().selectionChanged()
            setNeedsDisplay()
        case .changed:
            selection?.head = (col, row)
            setNeedsDisplay()
        case .ended:
            // Releasing copies the selection (like the desktop canvas).
            if let selection, let text = selectionText?(UInt16(selection.anchor.0), UInt16(selection.anchor.1), UInt16(selection.head.0), UInt16(selection.head.1)), !text.isEmpty {
                UIPasteboard.general.string = text
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onCopied?()
            }
        default:
            selection = nil
            setNeedsDisplay()
        }
    }

    @objc private func pinched(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            pinchStart = fontSize
        case .changed:
            fontSize = max(7, min(28, (pinchStart * g.scale * 2).rounded() / 2))
        case .ended:
            onFontSize?(fontSize)
        default:
            break
        }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }
    override var inputAccessoryView: UIView? { keyBar }

    var hasText: Bool { true }

    func insertText(_ text: String) {
        if stickyMods != 0, text.count == 1, text != "\n" {
            onKey?(.char(text: text.lowercased()), stickyMods)
            stickyMods = 0
            return
        }
        if text == "\n" {
            onKey?(.enter, stickyMods)
            stickyMods = 0
            return
        }
        onText?(text)
    }

    func deleteBackward() {
        onKey?(.backspace, stickyMods)
        stickyMods = 0
    }

    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardAppearance: UIKeyboardAppearance = .dark
    var keyboardType: UIKeyboardType = .default
    var returnKeyType: UIReturnKeyType = .default

    /// Hardware keyboards: arrows, Esc, Tab, function keys and Ctrl/Option combinations.
    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key, handle(key) else {
                unhandled.insert(press)
                continue
            }
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    private func handle(_ key: UIKey) -> Bool {
        var mods: UInt8 = 0
        if key.modifierFlags.contains(.shift) { mods |= 1 }
        if key.modifierFlags.contains(.alternate) { mods |= 2 }
        if key.modifierFlags.contains(.control) { mods |= 4 }
        if key.modifierFlags.contains(.command) {
            if key.charactersIgnoringModifiers == "v", let text = UIPasteboard.general.string {
                onPaste?(text)
                return true
            }
            return false
        }
        let special: KeyCode?
        switch key.keyCode {
        case .keyboardUpArrow: special = .up
        case .keyboardDownArrow: special = .down
        case .keyboardLeftArrow: special = .left
        case .keyboardRightArrow: special = .right
        case .keyboardEscape: special = .esc
        case .keyboardTab: special = mods & 1 != 0 ? .backTab : .tab
        case .keyboardReturnOrEnter, .keypadEnter: special = .enter
        case .keyboardDeleteOrBackspace: special = .backspace
        case .keyboardDeleteForward: special = .delete
        case .keyboardHome: special = .home
        case .keyboardEnd: special = .end
        case .keyboardPageUp: special = .pageUp
        case .keyboardPageDown: special = .pageDown
        case .keyboardInsert: special = .insert
        case .keyboardF1, .keyboardF2, .keyboardF3, .keyboardF4, .keyboardF5, .keyboardF6, .keyboardF7, .keyboardF8, .keyboardF9, .keyboardF10, .keyboardF11, .keyboardF12:
            special = .f(n: UInt8(key.keyCode.rawValue - UIKeyboardHIDUsage.keyboardF1.rawValue + 1))
        default: special = nil
        }
        if let special {
            onKey?(special, special == .backTab ? mods & ~1 : mods)
            return true
        }
        let chars = key.charactersIgnoringModifiers
        if mods & 6 != 0, chars.count == 1 {
            onKey?(.char(text: chars), mods)
            return true
        }
        return false
    }

    override func paste(_ sender: Any?) {
        if let text = UIPasteboard.general.string { onPaste?(text) }
    }

    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        action == #selector(paste(_:)) ? UIPasteboard.general.hasStrings : super.canPerformAction(action, withSender: sender)
    }
}

/// Cell flags of protocol 8 (`F_*`, `F8_*`).
enum Flag {
    static let bold: UInt32 = 1
    static let italic: UInt32 = 2
    static let underline: UInt32 = 4
    static let inverse: UInt32 = 8
    static let dim: UInt32 = 16
    static let hidden: UInt32 = 32
    static let strike: UInt32 = 64
    static let overline: UInt32 = 128
}

/// The monospaced fonts at one size and the cell they make.
struct Fonts {
    let size: CGFloat
    let regular: CTFont
    let bold: CTFont
    let italic: CTFont
    let boldItalic: CTFont
    let cellWidth: CGFloat
    let cellHeight: CGFloat
    let baseline: CGFloat
    let descent: CGFloat

    init(size: CGFloat) {
        self.size = size
        regular = UIFont.monospacedSystemFont(ofSize: size, weight: .regular) as CTFont
        bold = UIFont.monospacedSystemFont(ofSize: size, weight: .bold) as CTFont
        italic = CTFontCreateCopyWithSymbolicTraits(regular, size, nil, .traitItalic, .traitItalic) ?? regular
        boldItalic = CTFontCreateCopyWithSymbolicTraits(bold, size, nil, .traitItalic, .traitItalic) ?? bold
        var glyph = CGGlyph()
        var character: UniChar = 0x4D // "M"
        CTFontGetGlyphsForCharacters(regular, &character, &glyph, 1)
        var advance = CGSize.zero
        CTFontGetAdvancesForGlyphs(regular, .horizontal, &glyph, &advance, 1)
        cellWidth = advance.width
        let ascent = CTFontGetAscent(regular)
        descent = CTFontGetDescent(regular)
        let height = ceil(ascent + descent + max(CTFontGetLeading(regular), size * 0.15))
        cellHeight = height
        baseline = ((height - (ascent + descent)) / 2 + ascent).rounded()
    }

    func font(bold isBold: Bool, italic isItalic: Bool) -> CTFont {
        switch (isBold, isItalic) {
        case (true, true): return boldItalic
        case (true, false): return bold
        case (false, true): return italic
        default: return regular
        }
    }
}
