import SwiftUI
import UIKit

/// The desktop canvas' palette (canvas-core `paint`, canvas-gui `theme`):
/// safe for red/green colour blindness; states also differ by icon.
enum Palette {
    static let background = Color(rgb: 0x0D1117)
    static let panel = Color(rgb: 0x161B22)
    static let panelHi = Color(rgb: 0x21262D)
    static let border = Color(rgb: 0x30363D)
    static let text = Color(rgb: 0xC9D1D9)
    static let muted = Color(rgb: 0x6E7681)
    static let accent = Color(rgb: 0x58A6FF)
    static let positive = Color(rgb: 0x56B4E9)
    static let attention = Color(rgb: 0xFFB000)
    static let attentionBg = Color(rgb: 0x423006)
    static let finished = Color(rgb: 0x648FFF)
    static let finishedBg = Color(rgb: 0x1A244E)
    static let failed = Color(rgb: 0xE66100)
    static let purple = Color(rgb: 0xA371F7)
    static let gray = Color(rgb: 0x8B949E)
    static let dark = Color(rgb: 0x585E66)
    static let loop = Color(rgb: 0x009E73)
    /// Okabe-Ito group colours.
    static let groups: [Color] = [0x56B4E9, 0xE69F00, 0x20BE96, 0xCC79A7, 0xF0E442, 0xE66E28].map { Color(rgb: $0) }

    static func group(_ index: UInt8) -> Color {
        groups[Int(index) % groups.count]
    }
}

extension Color {
    init(rgb: UInt32, opacity: Double = 1) {
        self.init(.sRGB, red: Double((rgb >> 16) & 0xFF) / 255, green: Double((rgb >> 8) & 0xFF) / 255, blue: Double(rgb & 0xFF) / 255, opacity: opacity)
    }
}

extension StatusKind {
    var color: Color {
        switch self {
        case .working, .running, .preparing: return Palette.positive
        case .needsYou: return Palette.attention
        case .done: return Palette.finished
        case .idle: return Palette.gray
        case .elsewhere: return Palette.purple
        case .stopped, .unknown: return Palette.dark
        case .failed: return Palette.failed
        }
    }

    var symbol: String {
        switch self {
        case .working, .preparing: return "circle.dotted"
        case .needsYou: return "exclamationmark.bubble.fill"
        case .done: return "checkmark.circle.fill"
        case .idle: return "circle"
        case .running: return "circle.fill"
        case .elsewhere: return "arrow.up.right.square"
        case .stopped: return "stop.fill"
        case .failed: return "xmark.octagon.fill"
        case .unknown: return "questionmark.circle"
        }
    }

    var wantsAttention: Bool { self == .needsYou || self == .done }

    var animated: Bool { self == .working || self == .preparing }

    /// Sort order of the session list: what wants you first.
    var rank: Int {
        switch self {
        case .needsYou: return 0
        case .done: return 1
        case .failed: return 2
        case .working, .preparing: return 3
        case .running: return 4
        case .idle: return 5
        case .elsewhere: return 6
        case .stopped, .unknown: return 7
        }
    }
}

/// Terminal colours: the desktop's (Windows Terminal "Campbell" + the xterm cube).
enum TerminalColors {
    static let background: UInt32 = 0x0D1117
    static let foreground: UInt32 = 0xC9D1D9
    static let cursor: UInt32 = 0x58A6FF
    static let selection: UInt32 = 0x264F78
    /// Your own prompts in Copilot (navy background, blue bar).
    static let userBackground: UInt32 = 0x1E2C46
    static let userBar: UInt32 = 0x58A6FF

    private static let ansi: [UInt32] = [
        0x0C0C0C, 0xC50F1F, 0x13A10E, 0xC19C00, 0x0037DA, 0x881798, 0x3A96DD, 0xCCCCCC,
        0x767676, 0xE74856, 0x16C60C, 0xF9F1A5, 0x3B78FF, 0xB4009E, 0x61D6D6, 0xF2F2F2,
    ]

    static func indexed(_ index: Int) -> UInt32 {
        switch index {
        case 0...15:
            return ansi[index]
        case 16...231:
            let i = index - 16
            let level = { (v: Int) -> UInt32 in v == 0 ? 0 : UInt32(55 + v * 40) }
            return level(i / 36) << 16 | level((i / 6) % 6) << 8 | level(i % 6)
        default:
            let v = UInt32(8 + (index - 232) * 10)
            return v << 16 | v << 8 | v
        }
    }

    /// A protocol colour: 0 default, `0x0100_0000 | rgb`, else palette index + 1.
    static func resolve(_ value: UInt32, default fallback: UInt32) -> UInt32 {
        if value == 0 { return fallback }
        if value & 0x0100_0000 != 0 { return value & 0xFF_FFFF }
        return indexed(Int((value - 1) & 0xFF))
    }

    static func cg(_ rgb: UInt32, alpha: CGFloat = 1) -> CGColor {
        CGColor(srgbRed: CGFloat((rgb >> 16) & 0xFF) / 255, green: CGFloat((rgb >> 8) & 0xFF) / 255, blue: CGFloat(rgb & 0xFF) / 255, alpha: alpha)
    }

    static func ui(_ rgb: UInt32, alpha: CGFloat = 1) -> UIColor {
        UIColor(cgColor: cg(rgb, alpha: alpha))
    }
}
