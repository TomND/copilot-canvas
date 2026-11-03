import XCTest

/// The Notebook and Tasks places against the mock's demo notebook and todo
/// book (`canvas-mock` on 127.0.0.1:47600). Every change is undone or made on
/// something the test created under a unique name, so reruns against a
/// long-lived mock pass.
final class NotebookTasksUITests: XCTestCase {
    private let link = "ccanvas://pair?v=1&name=mock&url=ws%3A%2F%2F127%2E0%2E0%2E1%3A47600&code=ABCDEFGHJKMN"
    /// The first wait of a test: the app's launch, pairing and the first data.
    private let firstConnection: TimeInterval = 90

    override func setUp() {
        continueAfterFailure = false
    }

    private func launch(_ extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-noNotificationPrompt", "-noLock", "-pairLink", link, "-machine", "mock"] + extra
        app.launch()
        return app
    }

    private func button(_ app: XCUIApplication, startingWith text: String) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", text)).firstMatch
    }

    func testNotesOpenFollowLinksAndAreWrittenRenamedAndDeleted() throws {
        let app = launch(["-startMode", "notebook"])
        XCTAssertTrue(app.tabBars.buttons["Notebook"].waitForExistence(timeout: firstConnection), "the places' tab bar")
        let welcome = button(app, startingWith: "Welcome to the notebook")
        XCTAssertTrue(welcome.waitForExistence(timeout: firstConnection), "the notebook's root folder")
        XCTAssertTrue(button(app, startingWith: "Folder Groups").exists, "folders first")
        attach(app, "notebook")
        welcome.tap()
        // The note as its page: rendered Markdown, a [[link]] to another note.
        // A busy CI simulator's first web content process can take most of a minute.
        let page = app.webViews["notePage"].firstMatch
        XCTAssertTrue(page.waitForExistence(timeout: 60))
        XCTAssertTrue(page.staticTexts["Welcome to the notebook"].waitForExistence(timeout: 60), "the note's heading")
        XCTAssertTrue(app.descendants(matching: .any)["noteProvenance"].waitForExistence(timeout: 10))
        attach(app, "note")
        let wikiLink = page.links["Release plan"]
        XCTAssertTrue(wikiLink.waitForExistence(timeout: 10))
        wikiLink.tap()
        XCTAssertTrue(app.navigationBars["Release plan"].waitForExistence(timeout: 20), "the #note: link opened the note in the app")
        let provenance = app.descendants(matching: .any).matching(NSPredicate(format: "identifier == 'noteProvenance' AND label CONTAINS 'Copilot'")).firstMatch
        XCTAssertTrue(provenance.waitForExistence(timeout: 20), "written by an agent for you")
        attach(app, "linked-note")
        XCTAssertTrue(back(app, until: app.navigationBars["Welcome to the notebook"]), "back on the first note")
        // The toolbar's + menu (iOS 18 drops its identifier: its label too).
        let plus = menu(app, "newNoteMenu", label: "New note or folder")
        XCTAssertTrue(back(app, until: plus), "back in the notebook's folder")

        // A new note, written in the source editor and saved; then renamed and deleted.
        let name = "UI note \(Int.random(in: 1000...9999))"
        plus.tap()
        let newNote = app.buttons["New note"]
        XCTAssertTrue(newNote.waitForExistence(timeout: 10))
        newNote.tap()
        let nameField = app.alerts.firstMatch.textFields.firstMatch
        XCTAssertTrue(nameField.waitForExistence(timeout: 10))
        nameField.tap()
        nameField.typeText(name)
        app.alerts.firstMatch.buttons["Create"].tap()
        let source = app.textViews["noteSource"]
        XCTAssertTrue(source.waitForExistence(timeout: 20), "a new note opens in the editor")
        expectation(for: NSPredicate(format: "value CONTAINS %@", "# \(name)"), evaluatedWith: source)
        waitForExpectations(timeout: 10)
        XCTAssertTrue((source.value as? String ?? "").contains("audience: human"), "written as a note for you")
        XCTAssertTrue(waitForKeyboard(app, shown: true), "a new note takes the keyboard, the caret at its end")
        XCTAssertTrue(app.buttons["Bold"].exists && app.buttons["Undo"].exists, "the Markdown key row")
        let line = "Typed on the phone \(Int.random(in: 100...999))"
        app.typeText("\n\(line) ")
        app.buttons["Bold"].tap()
        app.typeText("now")
        attach(app, "note-editor")
        app.buttons["saveNote"].tap()
        let typed = app.webViews["notePage"].staticTexts.matching(NSPredicate(format: "label CONTAINS %@", line)).firstMatch
        XCTAssertTrue(typed.waitForExistence(timeout: 20), "the saved text on the page")
        XCTAssertTrue(app.webViews["notePage"].staticTexts["now"].exists, "bold from the key row")
        attach(app, "note-saved")
        // Delete it (to the trash), back to its folder: gone.
        let actions = menu(app, "noteActions", label: "Note actions")
        XCTAssertTrue(waitHittable(app, actions))
        actions.tap()
        let delete = app.buttons["Delete…"]
        XCTAssertTrue(delete.waitForExistence(timeout: 10))
        delete.tap()
        let confirm = app.buttons["Move to the trash"]
        XCTAssertTrue(confirm.waitForExistence(timeout: 10))
        confirm.tap()
        XCTAssertTrue(welcome.waitForExistence(timeout: 20), "back in the folder")
        let row = button(app, startingWith: name)
        waitGone(row, timeout: 15)
        XCTAssertFalse(row.exists, "the note went to the trash")
    }

    func testTasksDeclineAndReopenTickAndUntickCreateAndEditATodo() throws {
        let app = launch(["-startMode", "tasks"])
        XCTAssertTrue(app.buttons["boardPicker"].waitForExistence(timeout: firstConnection), "the boards")
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "needs you")).firstMatch.waitForExistence(timeout: 20), "what waits for you first")
        attach(app, "tasks")

        // A proposal: declined with a reason, then reopened (a proposal again).
        let proposal = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "needs you: proposal")).firstMatch
        if proposal.waitForExistence(timeout: 10) {
            proposal.tap()
            let decline = app.buttons["declineTodo"]
            XCTAssertTrue(decline.waitForExistence(timeout: 10))
            attach(app, "proposal")
            decline.tap()
            let why = app.alerts.firstMatch.textFields.firstMatch
            XCTAssertTrue(why.waitForExistence(timeout: 10))
            why.tap()
            why.typeText("Not now (UI test)")
            app.alerts.firstMatch.buttons["Decline"].tap()
            let status = app.staticTexts["todoStatus"]
            expectation(for: NSPredicate(format: "label == 'Canceled'"), evaluatedWith: status)
            waitForExpectations(timeout: 15)
            let reopen = app.buttons["reopenTodo"]
            XCTAssertTrue(waitHittable(app, reopen))
            reopen.tap()
            expectation(for: NSPredicate(format: "label == 'Proposed'"), evaluatedWith: status)
            waitForExpectations(timeout: 15)
            XCTAssertTrue(app.buttons["acceptTodo"].waitForExistence(timeout: 10), "a proposal again")
            XCTAssertTrue(back(app, until: app.buttons["newTodo"]))
        }

        // A checklist item ticked and unticked (WI-002 is in progress, further down).
        let momentum = button(app, startingWith: "WI-002,")
        XCTAssertTrue(scrollTo(app, momentum), "WI-002 on the Canvas App board")
        attach(app, "tasks-board")
        momentum.tap()
        let item = app.buttons.matching(NSPredicate(format: "identifier == 'checkItem' AND label == 'Stop on click'")).firstMatch
        XCTAssertTrue(item.waitForExistence(timeout: 20))
        let before = item.value as? String ?? ""
        let after = before == "done" ? "not done" : "done"
        item.tap()
        expectation(for: NSPredicate(format: "value == %@", after), evaluatedWith: item)
        waitForExpectations(timeout: 15)
        attach(app, "todo-detail")
        item.tap()
        expectation(for: NSPredicate(format: "value == %@", before), evaluatedWith: item)
        waitForExpectations(timeout: 15)
        let add = app.buttons["newTodo"]
        XCTAssertTrue(back(app, until: add))

        // A new todo, then edited; canceled at the end (reruns make new ones).
        let title = "UI todo \(Int.random(in: 1000...9999))"
        add.tap()
        let titleField = app.descendants(matching: .any).matching(identifier: "todoTitleField").firstMatch
        XCTAssertTrue(titleField.waitForExistence(timeout: 10))
        XCTAssertTrue(waitForKeyboard(app, shown: true), "the title has the keyboard")
        app.typeText(title)
        attach(app, "new-todo")
        app.buttons["addTodo"].tap()
        let shown = app.staticTexts["todoTitle"]
        XCTAssertTrue(shown.waitForExistence(timeout: 20), "the new todo's details")
        XCTAssertEqual(shown.label, title)
        app.buttons["editTodo"].tap()
        let labels = app.textFields["todoLabelsField"]
        XCTAssertTrue(labels.waitForExistence(timeout: 10))
        labels.tap()
        labels.typeText("ui-test")
        app.buttons["saveTodo"].tap()
        XCTAssertTrue(app.staticTexts["#ui-test"].waitForExistence(timeout: 15), "the edit shows")
        attach(app, "todo-edited")
        let cancel = app.buttons["cancelTodo"]
        XCTAssertTrue(waitHittable(app, cancel))
        cancel.tap()
        app.alerts.firstMatch.buttons["Cancel the todo"].tap()
        expectation(for: NSPredicate(format: "label == 'Canceled'"), evaluatedWith: app.staticTexts["todoStatus"])
        waitForExpectations(timeout: 15)
    }

    // MARK: Helpers

    /// A toolbar menu by its identifier, or by its label where iOS 18 drops the identifier.
    private func menu(_ app: XCUIApplication, _ id: String, label: String) -> XCUIElement {
        app.descendants(matching: .any).matching(NSPredicate(format: "identifier == %@ OR (label == %@ AND elementType == %d)", id, label, XCUIElement.ElementType.button.rawValue)).firstMatch
    }

    /// Back until `element` can be tapped: a tap on Back during a busy
    /// simulator's animation can be lost, so it is tapped again (never once
    /// `element` is there: at a place's first screen the bar's first button
    /// is the machine menu).
    private func back(_ app: XCUIApplication, until element: XCUIElement) -> Bool {
        for _ in 0..<4 {
            if waitHittable(app, element, timeout: 2) { return true }
            // A place's first screen: nothing to go back from.
            if app.buttons["machineMenu"].exists && app.buttons["machineMenu"].isHittable { return false }
            let button = app.navigationBars.buttons.element(boundBy: 0)
            if button.exists && button.isHittable { button.tap() }
            if waitHittable(app, element, timeout: 6) { return true }
        }
        return false
    }

    /// Scrolls the list down until `element` can be tapped (lists build rows as they scroll in).
    private func scrollTo(_ app: XCUIApplication, _ element: XCUIElement) -> Bool {
        let named = app.collectionViews["tasksList"]
        let list = named.exists ? named : app.collectionViews.firstMatch
        for _ in 0..<10 {
            if element.exists && element.isHittable { return true }
            list.swipeUp()
        }
        return element.exists && element.isHittable
    }

    private func waitGone(_ element: XCUIElement, timeout: TimeInterval = 5) {
        let deadline = Date().addingTimeInterval(timeout)
        while element.exists && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
    }

    /// Until `element` can be tapped (animations end late on a busy simulator).
    private func waitHittable(_ app: XCUIApplication, _ element: XCUIElement, timeout: TimeInterval = 15) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            dismissKeyboardTip(app)
            if element.exists && element.isHittable { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
        return false
    }

    /// iOS's one-time slide-to-type tip covers the keyboard on a fresh simulator.
    private func dismissKeyboardTip(_ app: XCUIApplication) {
        let tip = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "sliding your finger")).firstMatch
        if tip.exists, app.buttons["Continue"].exists {
            app.buttons["Continue"].tap()
        }
    }

    private func waitForKeyboard(_ app: XCUIApplication, shown: Bool, timeout: TimeInterval = 15) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            dismissKeyboardTip(app)
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
