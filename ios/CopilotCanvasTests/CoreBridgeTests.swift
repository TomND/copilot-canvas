import XCTest
@testable import CopilotCanvas

/// The Rust core through its Swift bindings, and the app's own small rules.
final class CoreBridgeTests: XCTestCase {
    func testPairingLinksAreReadByTheCore() throws {
        let link = try parsePairLink(uri: "ccanvas://pair?v=1&name=cloudpc&url=wss%3A%2F%2Fcopilot-canvas-47600.usw2.devtunnels.ms&code=abcd-efgh-jkmn&token=t&exp=5")
        XCTAssertEqual(link.url, "wss://copilot-canvas-47600.usw2.devtunnels.ms")
        XCTAssertEqual(link.code, "ABCDEFGHJKMN")
        XCTAssertEqual(link.token, "t")
        XCTAssertThrowsError(try parsePairLink(uri: "https://example.com"))
        XCTAssertEqual(normalizePairingCode(code: "abcd efgh jkmn"), "ABCDEFGHJKMN")
        XCTAssertNil(normalizePairingCode(code: "short"))
    }

    func testProtocolColoursResolveLikeTheDesktop() {
        XCTAssertEqual(TerminalColors.resolve(0, default: 0x123456), 0x123456)
        XCTAssertEqual(TerminalColors.resolve(0x0100_0000 | 0xFF8800, default: 0), 0xFF8800)
        XCTAssertEqual(TerminalColors.resolve(2, default: 0), 0xC50F1F, "palette index 1 (red)")
        XCTAssertEqual(TerminalColors.indexed(16), 0x000000)
        XCTAssertEqual(TerminalColors.indexed(231), 0xFFFFFF)
        XCTAssertEqual(TerminalColors.indexed(232), 0x080808)
    }

    func testWhatWantsYouIsListedFirst() {
        XCTAssertLessThan(StatusKind.needsYou.rank, StatusKind.done.rank)
        XCTAssertLessThan(StatusKind.done.rank, StatusKind.working.rank)
        XCTAssertLessThan(StatusKind.working.rank, StatusKind.idle.rank)
        XCTAssertTrue(StatusKind.needsYou.wantsAttention && StatusKind.done.wantsAttention)
    }

    func testCellsAreWholeAdvancesOfTheMonospacedFont() {
        let fonts = Fonts(size: 13)
        XCTAssertGreaterThan(fonts.cellWidth, 6)
        XCTAssertGreaterThan(fonts.cellHeight, fonts.cellWidth)
        XCTAssertGreaterThan(fonts.baseline, 0)
    }

    func testBoxDrawingCoversBordersAgentsDraw() {
        for scalar in ["─", "│", "╭", "╮", "╯", "╰", "┌", "┐", "└", "┘", "━", "┃", "═", "║", "╌", "█", "▀", "▄"] {
            XCTAssertTrue(BoxDrawing.handles(scalar.unicodeScalars.first!), scalar)
        }
        XCTAssertFalse(BoxDrawing.handles("a".unicodeScalars.first!))
    }
}
