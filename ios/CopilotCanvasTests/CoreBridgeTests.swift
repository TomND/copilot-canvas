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

    func testPathsAndLinksAreFoundUnderATap() {
        let line = "  Edited C:\\src\\app\\main.rs:42, see https://example.com/pr/7. Log: /var/log/app.log"
        let at = { (needle: String) in line.distance(from: line.startIndex, to: line.range(of: needle)!.lowerBound) + 1 }
        XCTAssertEqual(TerminalView.linkTarget(in: line, at: at("C:\\src")), "C:\\src\\app\\main.rs:42")
        XCTAssertEqual(TerminalView.linkTarget(in: line, at: at("https://")), "https://example.com/pr/7")
        XCTAssertEqual(TerminalView.linkTarget(in: line, at: at("/var/log")), "/var/log/app.log")
        XCTAssertNil(TerminalView.linkTarget(in: line, at: at("Edited")), "plain words are not links")
        XCTAssertNil(TerminalView.linkTarget(in: "a/b and c", at: 1), "relative paths are not opened")
        let target = FileTarget(path: "C:\\src\\app\\main.rs:42:7")
        XCTAssertEqual(target.path, "C:\\src\\app\\main.rs")
        XCTAssertEqual(target.line, 42)
        XCTAssertEqual(FileTarget(path: "/etc/hosts").line, nil)
    }

    func testPicturesAreSentAsBoundedPNGs() throws {
        let big = UIGraphicsImageRenderer(size: CGSize(width: 4000, height: 1000)).image { context in
            UIColor.orange.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 4000, height: 1000))
        }
        let png = try XCTUnwrap(TerminalScreen.png(big))
        let image = try XCTUnwrap(UIImage(data: png))
        XCTAssertEqual(max(image.size.width * image.scale, image.size.height * image.scale), 2048, accuracy: 1)
        XCTAssertEqual(png.prefix(4), Data([0x89, 0x50, 0x4E, 0x47]))
    }

    func testPushTokensAreHexAndTheEnvironmentIsKnown() {
        XCTAssertEqual(PushRegistration.hex(Data([0x00, 0xab, 0xff])), "00abff")
        XCTAssertTrue(["sandbox", "production"].contains(PushRegistration.environment))
        XCTAssertThrowsError(try openTunnelAccess(secret: Data(repeating: 1, count: 32), nonce: "AAAA", sealed: "AAAA"), "a message not sealed for this phone")
    }

    func testBoxDrawingCoversBordersAgentsDraw() {
        for scalar in ["─", "│", "╭", "╮", "╯", "╰", "┌", "┐", "└", "┘", "━", "┃", "═", "║", "╌", "█", "▀", "▄"] {
            XCTAssertTrue(BoxDrawing.handles(scalar.unicodeScalars.first!), scalar)
        }
        XCTAssertFalse(BoxDrawing.handles("a".unicodeScalars.first!))
    }
}
