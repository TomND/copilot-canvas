import XCTest

/// The whole path on a simulator: the app pairs with the mock service
/// (`cargo run -p canvas-mock` on 127.0.0.1:47600), lists its sessions,
/// starts one, types into its shell through the keyboard and sees the echo.
final class TerminalUITests: XCTestCase {
    private let link = "ccanvas://pair?v=1&name=mock&url=ws%3A%2F%2F127%2E0%2E0%2E1%3A47600&code=ABCDEFGHJKMN"

    override func setUp() {
        continueAfterFailure = false
    }

    private func launch(_ extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-noNotificationPrompt", "-noLock", "-pairLink", link] + extra
        app.launch()
        return app
    }

    func testSessionsAreListedByWhatTheyNeed() throws {
        let app = launch()
        XCTAssertTrue(app.staticTexts["Fix flaky test"].waitForExistence(timeout: 45), "the canvas arrived")
        XCTAssertTrue(app.staticTexts["Scratch"].exists)
        attach(app, "sessions")
        app.buttons["Canvas"].tap()
        sleep(1)
        attach(app, "canvas")
    }

    func testTypingInATerminalReachesTheShell() throws {
        let app = launch(["-openSession", "mock-loose"])
        let terminal = app.descendants(matching: .any).matching(identifier: "terminal").firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 45))
        // The session starts: wait for the shell's prompt (zsh `%`, bash `$`, root `#`).
        let prompt = NSPredicate(format: "value MATCHES %@", "(?s).*[%$#>][ \\n]*")
        expectation(for: prompt, evaluatedWith: terminal)
        waitForExpectations(timeout: 45)
        terminal.tap()
        app.typeText("echo ui-$((6*7))-ok\n")
        let echoed = NSPredicate(format: "value CONTAINS 'ui-42-ok'")
        expectation(for: echoed, evaluatedWith: terminal)
        waitForExpectations(timeout: 45)
        attach(app, "terminal")
        // The key bar sends keys the keyboard lacks.
        XCTAssertTrue(app.buttons["esc"].firstMatch.exists)
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
