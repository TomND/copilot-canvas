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
        // The key bar sends keys the keyboard lacks; its hide button is always there.
        XCTAssertTrue(app.buttons["esc"].firstMatch.exists)
        let hide = app.buttons["Hide keyboard"].firstMatch
        XCTAssertTrue(hide.isHittable, "the hide button is pinned, not scrolled away")
        hide.tap()
        XCTAssertTrue(waitForKeyboard(app, shown: false), "the hide button closes the keyboard")
        terminal.tap()
        XCTAssertTrue(waitForKeyboard(app, shown: true), "a tap on the terminal opens it again")
    }

    func testTheKeyboardStaysOutOfTheWayOfTheNewSessionForm() throws {
        let app = launch()
        let add = app.buttons["New session"]
        XCTAssertTrue(add.waitForExistence(timeout: 45))
        expectation(for: NSPredicate(format: "isEnabled == true"), evaluatedWith: add)
        waitForExpectations(timeout: 45)
        add.tap()
        let name = app.textFields["Name (default: the folder's name)"]
        XCTAssertTrue(name.waitForExistence(timeout: 10))
        name.tap()
        name.typeText("Keyboard check")
        XCTAssertTrue(waitForKeyboard(app, shown: true))
        // Picking a folder closes the keyboard, and it stays closed afterwards.
        app.buttons["browse"].tap()
        let use = app.buttons["useFolder"]
        XCTAssertTrue(use.waitForExistence(timeout: 20))
        use.tap()
        XCTAssertTrue(app.buttons["browse"].waitForExistence(timeout: 10))
        XCTAssertTrue(waitForKeyboard(app, shown: false), "no keyboard after picking a folder")
        // The prompt can be typed in, then the Done button hides the keyboard.
        let prompt = app.textViews["prompt"]
        XCTAssertTrue(prompt.isHittable, "the prompt is reachable")
        prompt.tap()
        prompt.typeText("Explain this repository")
        XCTAssertTrue(waitForKeyboard(app, shown: true))
        attach(app, "new-session-typing")
        app.buttons["Hide keyboard"].firstMatch.tap()
        XCTAssertTrue(waitForKeyboard(app, shown: false), "Done closes the keyboard")
        // A tap outside the fields closes it too.
        name.tap()
        XCTAssertTrue(waitForKeyboard(app, shown: true))
        app.navigationBars["New session"].coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(waitForKeyboard(app, shown: false), "a tap outside closes the keyboard")
        XCTAssertEqual(prompt.value as? String, "Explain this repository")
        attach(app, "new-session")
        app.buttons["Cancel"].tap()
    }

    private func waitForKeyboard(_ app: XCUIApplication, shown: Bool, timeout: TimeInterval = 5) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if app.keyboards.firstMatch.exists == shown { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
        return false
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
