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
        app.launchArguments = ["-noNotificationPrompt", "-noLock", "-pairLink", link, "-machine", "mock"] + extra
        app.launch()
        return app
    }

    func testSessionsAreListedByWhatTheyNeed() throws {
        let app = launch()
        XCTAssertTrue(app.staticTexts["Fix flaky test"].waitForExistence(timeout: 45), "the canvas arrived")
        // Lists build rows as they scroll in: other sessions may be further down.
        XCTAssertTrue(app.buttons["machineMenu"].exists)
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

    /// Needs a second mock: `canvas-mock --listen 127.0.0.1:47601 --state /tmp/canvas-mock-personal`.
    func testTwoMachinesSwitchAndShowWhatNeedsYouOnTheOther() throws {
        let personal = "ccanvas://pair?v=1&name=personal&url=ws%3A%2F%2F127%2E0%2E0%2E1%3A47601&code=ABCDEFGHJKMN"
        let app = XCUIApplication()
        app.launchArguments = ["-noNotificationPrompt", "-noLock", "-pairLink", link, "-pairLink", personal, "-machine", "mock"]
        app.launch()
        let menu = app.buttons["machineMenu"]
        XCTAssertTrue(menu.waitForExistence(timeout: 45))
        expectation(for: NSPredicate(format: "label ENDSWITH ': mock'"), evaluatedWith: menu)
        waitForExpectations(timeout: 45)
        // What wants you on the other machine shows at the top.
        let fromPersonal = app.buttons.matching(identifier: "attention-chip").matching(NSPredicate(format: "label BEGINSWITH 'personal:'")).firstMatch
        XCTAssertTrue(fromPersonal.waitForExistence(timeout: 45), "the other machine's sessions that want you")
        attach(app, "two-machines")
        // The switcher.
        menu.tap()
        let item = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'personal'")).firstMatch
        XCTAssertTrue(item.waitForExistence(timeout: 10))
        item.tap()
        expectation(for: NSPredicate(format: "label ENDSWITH ': personal'"), evaluatedWith: menu)
        waitForExpectations(timeout: 20)
        XCTAssertTrue(app.staticTexts["Fix flaky test"].waitForExistence(timeout: 45), "the other machine's sessions")
        let fromMock = app.buttons.matching(identifier: "attention-chip").matching(NSPredicate(format: "label BEGINSWITH 'mock:'")).firstMatch
        XCTAssertTrue(fromMock.waitForExistence(timeout: 45))
        attach(app, "switched")
        // A chip switches back and opens its session there.
        fromMock.tap()
        let terminal = app.descendants(matching: .any).matching(identifier: "terminal").firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 30), "the session opened")
        attach(app, "opened-on-mock")
        // Back on the list: the machine is the chip's.
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(menu.waitForExistence(timeout: 10))
        expectation(for: NSPredicate(format: "label ENDSWITH ': mock'"), evaluatedWith: menu)
        waitForExpectations(timeout: 20)
    }

    func testASubsessionStartsNextToItsParent() throws {
        let app = launch()
        let scratch = app.staticTexts["Scratch"].firstMatch
        XCTAssertTrue(scratch.waitForExistence(timeout: 45))
        scratch.press(forDuration: 1.0)
        let item = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'New subsession'")).firstMatch
        XCTAssertTrue(item.waitForExistence(timeout: 10))
        item.tap()
        XCTAssertTrue(app.navigationBars["New subsession"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.staticTexts["Scratch"].exists, "the parent is named")
        let name = app.textFields["Name (default: the folder's name)"]
        let title = "Scratch follow-up \(Int.random(in: 100...999))"
        name.tap()
        name.typeText(title)
        attach(app, "new-subsession")
        app.buttons["Start"].tap()
        let terminal = app.descendants(matching: .any).matching(identifier: "terminal").firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 45), "the subsession's terminal opened")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        // Rows are built as they scroll in: scroll to it.
        let row = app.staticTexts[title]
        for _ in 0..<8 where !row.exists {
            app.collectionViews.firstMatch.swipeUp()
        }
        XCTAssertTrue(row.waitForExistence(timeout: 10), "listed")
        XCTAssertTrue(app.staticTexts["↳"].exists, "shown as a subsession")
    }

    func testPassiveSessionsPauseAndResume() throws {
        let app = launch()
        let drift = app.staticTexts["Drift guard"].firstMatch
        XCTAssertTrue(drift.waitForExistence(timeout: 45))
        let pause = app.buttons.matching(identifier: "Pause").firstMatch
        let resume = app.buttons.matching(identifier: "Resume").firstMatch
        drift.tap()
        XCTAssertTrue(pause.waitForExistence(timeout: 10) || resume.exists, "its actions")
        attach(app, "passive-actions")
        if resume.exists {
            // Left paused by an earlier run.
            resume.tap()
            drift.tap()
        }
        XCTAssertTrue(pause.waitForExistence(timeout: 10))
        pause.tap()
        // The actions follow its state: paused now, so it offers to resume.
        drift.tap()
        XCTAssertTrue(resume.waitForExistence(timeout: 20), "paused")
        resume.tap()
        drift.tap()
        XCTAssertTrue(pause.waitForExistence(timeout: 20), "resumed")
        let cancel = app.buttons.matching(identifier: "Cancel").firstMatch
        if cancel.exists { cancel.tap() }
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
