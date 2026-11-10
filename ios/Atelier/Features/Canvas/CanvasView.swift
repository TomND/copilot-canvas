import SwiftUI

/// The canvas as on the desktop: groups, session boxes with their status
/// borders, subagents and subsessions linked to their parent. Drag to pan,
/// pinch to zoom, tap a box to open its terminal.
struct CanvasView: View {
    @ObservedObject var connection: ServiceConnection
    var open: (String) -> Void

    /// Points per canvas cell (the desktop's grid is in terminal cells).
    private let cellW: CGFloat = 8
    private let cellH: CGFloat = 17
    @State private var scale: CGFloat = 1
    @State private var offset: CGSize = .zero
    @State private var baseScale: CGFloat = 1
    @State private var baseOffset: CGSize = .zero
    @State private var fitted = false
    @State private var selected: String?

    var body: some View {
        GeometryReader { geometry in
            let overview = connection.overview
            TimelineView(.animation(minimumInterval: 1.0 / 12, paused: !(overview?.sessions.contains { $0.status.animated } ?? false))) { timeline in
                Canvas { ctx, size in
                    guard let overview else { return }
                    draw(overview, in: &ctx, size: size, time: timeline.date.timeIntervalSinceReferenceDate)
                }
            }
            .background(Palette.background)
            .contentShape(Rectangle())
            .gesture(pan.simultaneously(with: zoom))
            .onTapGesture { location in tap(at: location) }
            .onAppear { fitIfNeeded(geometry.size) }
            .onChange(of: geometry.size) { _, size in fitIfNeeded(size) }
            .onChange(of: connection.overview?.sessions.count ?? 0) { _, _ in fitIfNeeded(geometry.size) }
            .overlay(alignment: .bottomTrailing) {
                Button {
                    if let overview { withAnimation(.easeInOut(duration: 0.25)) { fit(overview, in: geometry.size) } }
                } label: {
                    Image(systemName: "arrow.up.left.and.arrow.down.right").padding(10)
                }
                .buttonStyle(.bordered)
                .padding(16)
                .accessibilityLabel("Fit the canvas")
            }
        }
    }

    // MARK: Gestures

    private var pan: some Gesture {
        DragGesture(minimumDistance: 4)
            .onChanged { value in
                offset = CGSize(width: baseOffset.width + value.translation.width, height: baseOffset.height + value.translation.height)
            }
            .onEnded { _ in baseOffset = offset }
    }

    private var zoom: some Gesture {
        MagnificationGesture()
            .onChanged { value in scale = max(0.2, min(3, baseScale * value)) }
            .onEnded { _ in baseScale = scale }
    }

    private func tap(at point: CGPoint) {
        guard let overview = connection.overview else { return }
        let hit = overview.sessions.last { item in rect(item).contains(point) }
        guard let hit else { selected = nil; return }
        selected = hit.id
        switch hit.kind {
        case .session: open(hit.id)
        case .subagent: if let parent = hit.parent { open(parent) }
        default: break
        }
    }

    // MARK: Geometry

    private func toScreen(_ x: Int32, _ y: Int32) -> CGPoint {
        CGPoint(x: CGFloat(x) * cellW * scale + offset.width, y: CGFloat(y) * cellH * scale + offset.height)
    }

    private func rect(_ item: SessionItem) -> CGRect {
        let origin = toScreen(item.x, item.y)
        return CGRect(x: origin.x, y: origin.y, width: CGFloat(item.w) * cellW * scale, height: CGFloat(item.h) * cellH * scale)
    }

    private func rect(_ group: GroupItem) -> CGRect {
        let origin = toScreen(group.x, group.y)
        return CGRect(x: origin.x, y: origin.y, width: CGFloat(group.w) * cellW * scale, height: CGFloat(group.h) * cellH * scale)
    }

    /// The first time there is a canvas and a size, everything is shown.
    private func fitIfNeeded(_ size: CGSize) {
        guard !fitted, size.width > 0, size.height > 0 else { return }
        DispatchQueue.main.async {
            guard !fitted, let overview = connection.overview, !overview.sessions.isEmpty else { return }
            fit(overview, in: size)
        }
    }

    private func fit(_ overview: Overview, in size: CGSize) {
        let rects = overview.sessions.map { CGRect(x: CGFloat($0.x) * cellW, y: CGFloat($0.y) * cellH, width: CGFloat($0.w) * cellW, height: CGFloat($0.h) * cellH) }
            + overview.groups.map { CGRect(x: CGFloat($0.x) * cellW, y: CGFloat($0.y) * cellH, width: CGFloat($0.w) * cellW, height: CGFloat($0.h) * cellH) }
        guard let first = rects.first, size.width > 0 else { return }
        let bounds = rects.dropFirst().reduce(first) { $0.union($1) }.insetBy(dx: -16, dy: -16)
        let s = max(0.2, min(1.5, min(size.width / bounds.width, size.height / bounds.height)))
        scale = s
        baseScale = s
        // Centred across; from the top on a tall phone.
        let top = bounds.height * s < size.height ? CGFloat(12) : (size.height - bounds.height * s) / 2
        offset = CGSize(width: -bounds.minX * s + (size.width - bounds.width * s) / 2, height: -bounds.minY * s + top)
        baseOffset = offset
        fitted = true
    }

    // MARK: Drawing

    private func draw(_ overview: Overview, in ctx: inout GraphicsContext, size: CGSize, time: Double) {
        // Dot grid.
        if scale > 0.45 {
            let step = CGFloat(4) * cellW * scale * 2
            var x = offset.width.truncatingRemainder(dividingBy: step)
            while x < size.width {
                var y = offset.height.truncatingRemainder(dividingBy: step)
                while y < size.height {
                    ctx.fill(Path(ellipseIn: CGRect(x: x, y: y, width: 1.5, height: 1.5)), with: .color(Palette.border))
                    y += step
                }
                x += step
            }
        }
        for group in overview.groups {
            let r = rect(group)
            let color = Palette.group(group.color)
            ctx.fill(Path(roundedRect: r, cornerRadius: 10 * scale), with: .color(color.opacity(0.05)))
            ctx.stroke(Path(roundedRect: r, cornerRadius: 10 * scale), with: .color(color.opacity(0.55)), style: StrokeStyle(lineWidth: 1, dash: [5, 4]))
            let members = overview.sessions.filter { $0.group == group.id && $0.kind != .subagent }.count
            ctx.draw(Text("▣ \(group.name) · \(members)").font(.system(size: max(9, 13 * scale), weight: .semibold)).foregroundColor(color), at: CGPoint(x: r.minX + 10 * scale, y: r.minY + 9 * scale), anchor: .leading)
        }
        // Edges: subagents (solid, orthogonal) and subsessions (dotted).
        let byId = Dictionary(uniqueKeysWithValues: overview.sessions.map { ($0.id, $0) })
        for item in overview.sessions {
            guard let parentId = item.parent, let parent = byId[parentId] else { continue }
            let from = rect(parent)
            let to = rect(item)
            var path = Path()
            if item.kind == .subagent {
                let x = from.minX + 12 * scale
                path.move(to: CGPoint(x: x, y: from.maxY))
                path.addLine(to: CGPoint(x: x, y: to.midY))
                path.addLine(to: CGPoint(x: to.minX, y: to.midY))
                ctx.stroke(path, with: .color(Palette.muted), lineWidth: 1)
            } else {
                path.move(to: CGPoint(x: from.midX, y: from.maxY))
                path.addLine(to: CGPoint(x: from.midX, y: (from.maxY + to.minY) / 2))
                path.addLine(to: CGPoint(x: to.midX, y: (from.maxY + to.minY) / 2))
                path.addLine(to: CGPoint(x: to.midX, y: to.minY))
                ctx.stroke(path, with: .color(Palette.accent.opacity(0.8)), style: StrokeStyle(lineWidth: 1.2, dash: [3, 3]))
            }
        }
        for item in overview.sessions {
            drawBox(item, in: &ctx, time: time)
        }
    }

    private func drawBox(_ item: SessionItem, in ctx: inout GraphicsContext, time: Double) {
        let r = rect(item)
        let radius = 7 * scale
        let shape = Path(roundedRect: r, cornerRadius: radius)
        let status = item.status
        var fill = Palette.panel
        if status == .needsYou { fill = Palette.attentionBg } else if status == .done { fill = Palette.finishedBg }
        ctx.fill(shape, with: .color(fill))
        // Borders: gold heavy for "needs you", blue double for "done · review", dashed for passive sessions.
        switch status {
        case .needsYou:
            ctx.stroke(shape, with: .color(Palette.attention), lineWidth: 2.5)
        case .done:
            ctx.stroke(shape, with: .color(Palette.finished), lineWidth: 1.2)
            ctx.stroke(Path(roundedRect: r.insetBy(dx: 3, dy: 3), cornerRadius: max(1, radius - 3)), with: .color(Palette.finished), lineWidth: 1.2)
        default:
            let dashed = item.kind == .passive
            ctx.stroke(shape, with: .color(item.alive ? Palette.text.opacity(0.55) : Palette.border), style: StrokeStyle(lineWidth: 1, dash: dashed ? [4, 3] : []))
        }
        if selected == item.id {
            ctx.stroke(Path(roundedRect: r.insetBy(dx: -3, dy: -3), cornerRadius: radius + 3), with: .color(Palette.accent), lineWidth: 2)
        }
        guard scale > 0.3 else {
            // Zoomed out: just the status colour.
            ctx.fill(Path(ellipseIn: CGRect(x: r.midX - 3, y: r.midY - 3, width: 6, height: 6)), with: .color(status.color))
            return
        }
        let size = max(7, 12 * scale)
        // Clear of the double border of "done · review" boxes.
        let inset = 8 * scale + (status == .done ? 4 : status == .needsYou ? 2 : 0)
        let prefix = item.kind == .subagent ? "◇ " : item.kind == .automation ? "◷ " : item.kind == .passive ? "⟲ " : item.parent != nil ? "↳ " : ""
        var ctxText = ctx
        ctxText.clip(to: Path(roundedRect: r.insetBy(dx: inset / 2, dy: 1), cornerRadius: radius))
        ctxText.draw(Text(prefix + item.name).font(.system(size: size, weight: .semibold)).foregroundColor(Palette.text), at: CGPoint(x: r.minX + inset, y: r.minY + cellH * scale * 0.75), anchor: .leading)
        if item.h >= 3 {
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            let icon = status.animated ? spinner[Int(time * 12) % spinner.count] : statusGlyph(status)
            var line = "\(icon) \(item.statusLabel)"
            if !item.loopLabel.isEmpty { line += "  ↻ \(item.loopLabel)" }
            ctxText.draw(Text(line).font(.system(size: size * 0.9)).foregroundColor(status.color), at: CGPoint(x: r.minX + inset, y: r.minY + cellH * scale * 1.75), anchor: .leading)
        }
        if item.h >= 4, !item.subtitle.isEmpty {
            ctxText.draw(Text(item.subtitle).font(.system(size: size * 0.85)).foregroundColor(Palette.muted), at: CGPoint(x: r.minX + inset, y: r.minY + cellH * scale * 2.75), anchor: .leading)
        }
    }

    private func statusGlyph(_ status: StatusKind) -> String {
        switch status {
        case .needsYou: return "◆"
        case .done: return "✓"
        case .idle: return "○"
        case .running: return "●"
        case .elsewhere: return "↗"
        case .stopped: return "■"
        case .failed: return "✗"
        default: return "·"
        }
    }
}
