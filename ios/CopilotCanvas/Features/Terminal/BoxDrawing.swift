import CoreGraphics

/// Box-drawing and block characters drawn as geometry, so borders join
/// across cells whatever the font's line height (as Ghostty does).
enum BoxDrawing {
    /// Line weights towards (up, right, down, left): 0 none, 1 light, 2 heavy, 3 double.
    private static let lines: [UInt32: (UInt8, UInt8, UInt8, UInt8)] = [
        0x2500: (0, 1, 0, 1), 0x2501: (0, 2, 0, 2), 0x2502: (1, 0, 1, 0), 0x2503: (2, 0, 2, 0),
        0x250C: (0, 1, 1, 0), 0x250F: (0, 2, 2, 0), 0x2510: (0, 0, 1, 1), 0x2513: (0, 0, 2, 2),
        0x2514: (1, 1, 0, 0), 0x2517: (2, 2, 0, 0), 0x2518: (1, 0, 0, 1), 0x251B: (2, 0, 0, 2),
        0x251C: (1, 1, 1, 0), 0x2523: (2, 2, 2, 0), 0x2524: (1, 0, 1, 1), 0x252B: (2, 0, 2, 2),
        0x252C: (0, 1, 1, 1), 0x2533: (0, 2, 2, 2), 0x2534: (1, 1, 0, 1), 0x253B: (2, 2, 0, 2),
        0x253C: (1, 1, 1, 1), 0x254B: (2, 2, 2, 2),
        0x2550: (0, 3, 0, 3), 0x2551: (3, 0, 3, 0), 0x2554: (0, 3, 3, 0), 0x2557: (0, 0, 3, 3),
        0x255A: (3, 3, 0, 0), 0x255D: (3, 0, 0, 3), 0x2560: (3, 3, 3, 0), 0x2563: (3, 0, 3, 3),
        0x2566: (0, 3, 3, 3), 0x2569: (3, 3, 0, 3), 0x256C: (3, 3, 3, 3),
        0x2574: (0, 0, 0, 1), 0x2575: (1, 0, 0, 0), 0x2576: (0, 1, 0, 0), 0x2577: (0, 0, 1, 0),
        0x2578: (0, 0, 0, 2), 0x2579: (2, 0, 0, 0), 0x257A: (0, 2, 0, 0), 0x257B: (0, 0, 2, 0),
    ]
    /// Dashed lines: (horizontal, heavy, dashes).
    private static let dashed: [UInt32: (Bool, Bool, Int)] = [
        0x2504: (true, false, 3), 0x2505: (true, true, 3), 0x2506: (false, false, 3), 0x2507: (false, true, 3),
        0x2508: (true, false, 4), 0x2509: (true, true, 4), 0x250A: (false, false, 4), 0x250B: (false, true, 4),
        0x254C: (true, false, 2), 0x254D: (true, true, 2), 0x254E: (false, false, 2), 0x254F: (false, true, 2),
    ]

    static func handles(_ scalar: Unicode.Scalar) -> Bool {
        let v = scalar.value
        return lines[v] != nil || dashed[v] != nil || (0x256D...0x2570).contains(v) || (0x2580...0x2590).contains(v) || (0x2591...0x2593).contains(v)
    }

    static func draw(_ scalar: Unicode.Scalar, in rect: CGRect, color: CGColor, ctx: CGContext) {
        let v = scalar.value
        let light = max(1, (rect.width / 8).rounded())
        let heavy = light * 2
        let cx = (rect.midX * 2).rounded() / 2
        let cy = (rect.midY * 2).rounded() / 2
        ctx.saveGState()
        defer { ctx.restoreGState() }
        ctx.setFillColor(color)
        ctx.setStrokeColor(color)
        if let (up, right, down, left) = lines[v] {
            func stroke(_ weight: UInt8, _ from: CGPoint, _ to: CGPoint, vertical: Bool) {
                guard weight > 0 else { return }
                if weight == 3 {
                    let gap = light * 1.5
                    for offset in [-gap, gap] {
                        let a = vertical ? CGPoint(x: from.x + offset, y: from.y) : CGPoint(x: from.x, y: from.y + offset)
                        let b = vertical ? CGPoint(x: to.x + offset, y: to.y) : CGPoint(x: to.x, y: to.y + offset)
                        bar(a, b, light, ctx)
                    }
                } else {
                    bar(from, to, weight == 2 ? heavy : light, ctx)
                }
            }
            let extendH = max(up, down) == 3 ? light * 1.5 : max(up, down) == 2 ? heavy / 2 : light / 2
            let extendV = max(left, right) == 3 ? light * 1.5 : max(left, right) == 2 ? heavy / 2 : light / 2
            stroke(up, CGPoint(x: cx, y: rect.minY), CGPoint(x: cx, y: cy + extendV), vertical: true)
            stroke(down, CGPoint(x: cx, y: cy - extendV), CGPoint(x: cx, y: rect.maxY), vertical: true)
            stroke(left, CGPoint(x: rect.minX, y: cy), CGPoint(x: cx + extendH, y: cy), vertical: false)
            stroke(right, CGPoint(x: cx - extendH, y: cy), CGPoint(x: rect.maxX, y: cy), vertical: false)
            return
        }
        if let (horizontal, isHeavy, count) = dashed[v] {
            let width = isHeavy ? heavy : light
            let span = horizontal ? rect.width : rect.height
            let segment = span / CGFloat(count)
            for i in 0..<count {
                let start = CGFloat(i) * segment + segment * 0.15
                let end = CGFloat(i) * segment + segment * 0.85
                if horizontal {
                    bar(CGPoint(x: rect.minX + start, y: cy), CGPoint(x: rect.minX + end, y: cy), width, ctx)
                } else {
                    bar(CGPoint(x: cx, y: rect.minY + start), CGPoint(x: cx, y: rect.minY + end), width, ctx)
                }
            }
            return
        }
        switch v {
        case 0x256D...0x2570:
            // Rounded corners: ╭ ╮ ╯ ╰
            let path = CGMutablePath()
            let r = min(rect.width, rect.height) / 2
            switch v {
            case 0x256D:
                path.move(to: CGPoint(x: cx, y: rect.maxY))
                path.addLine(to: CGPoint(x: cx, y: cy + r))
                path.addQuadCurve(to: CGPoint(x: cx + r, y: cy), control: CGPoint(x: cx, y: cy))
                path.addLine(to: CGPoint(x: rect.maxX, y: cy))
            case 0x256E:
                path.move(to: CGPoint(x: rect.minX, y: cy))
                path.addLine(to: CGPoint(x: cx - r, y: cy))
                path.addQuadCurve(to: CGPoint(x: cx, y: cy + r), control: CGPoint(x: cx, y: cy))
                path.addLine(to: CGPoint(x: cx, y: rect.maxY))
            case 0x256F:
                path.move(to: CGPoint(x: cx, y: rect.minY))
                path.addLine(to: CGPoint(x: cx, y: cy - r))
                path.addQuadCurve(to: CGPoint(x: cx - r, y: cy), control: CGPoint(x: cx, y: cy))
                path.addLine(to: CGPoint(x: rect.minX, y: cy))
            default:
                path.move(to: CGPoint(x: cx, y: rect.minY))
                path.addLine(to: CGPoint(x: cx, y: cy - r))
                path.addQuadCurve(to: CGPoint(x: cx + r, y: cy), control: CGPoint(x: cx, y: cy))
                path.addLine(to: CGPoint(x: rect.maxX, y: cy))
            }
            ctx.setLineWidth(light)
            ctx.addPath(path)
            ctx.strokePath()
        case 0x2580: ctx.fill(CGRect(x: rect.minX, y: rect.minY, width: rect.width, height: rect.height / 2))
        case 0x2581...0x2588:
            let eighths = CGFloat(v - 0x2580)
            let h = rect.height * eighths / 8
            ctx.fill(CGRect(x: rect.minX, y: rect.maxY - h, width: rect.width, height: h))
        case 0x2589...0x258F:
            let eighths = CGFloat(0x2590 - v)
            ctx.fill(CGRect(x: rect.minX, y: rect.minY, width: rect.width * eighths / 8, height: rect.height))
        case 0x2590: ctx.fill(CGRect(x: rect.midX, y: rect.minY, width: rect.width / 2, height: rect.height))
        case 0x2591...0x2593:
            ctx.setFillColor(color.copy(alpha: (color.alpha) * CGFloat(v - 0x2590) * 0.25) ?? color)
            ctx.fill(rect)
        default:
            break
        }
    }

    private static func bar(_ from: CGPoint, _ to: CGPoint, _ width: CGFloat, _ ctx: CGContext) {
        if from.x == to.x {
            ctx.fill(CGRect(x: from.x - width / 2, y: min(from.y, to.y), width: width, height: abs(to.y - from.y)))
        } else {
            ctx.fill(CGRect(x: min(from.x, to.x), y: from.y - width / 2, width: abs(to.x - from.x), height: width))
        }
    }
}
