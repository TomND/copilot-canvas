import Network
import WebKit
import XCTest
@testable import CopilotCanvas

/// A local TCP port that counts the connections it gets: whatever a report
/// page tries to load must never reach it.
private final class Probe: @unchecked Sendable {
    private let listener: NWListener
    private let lock = NSLock()
    private var count = 0

    init() throws {
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] connection in
            self?.lock.withLock { self?.count += 1 }
            connection.start(queue: .global())
            connection.cancel()
        }
    }

    var connections: Int { lock.withLock { count } }
    var port: UInt16 { listener.port?.rawValue ?? 0 }

    func start() async {
        let ready = AsyncStream<Void> { continuation in
            listener.stateUpdateHandler = { state in
                if case .ready = state { continuation.yield() }
            }
        }
        listener.start(queue: .global())
        for await _ in ready { break }
    }

    func stop() { listener.cancel() }
}

/// The report view's lock-down (`ReportLockdown`), on raw pages that try
/// everything (like the desktop's parity row 41b), and the core's pages.
@MainActor
final class ReportViewTests: XCTestCase {
    private func load(_ controller: ReportWebController, _ html: String) async throws {
        let finished = expectation(description: "the page loaded")
        controller.onFinish = { finished.fulfill() }
        controller.load(html)
        await fulfillment(of: [finished], timeout: 15)
        controller.onFinish = nil
    }

    private func eval(_ controller: ReportWebController, _ script: String) async -> Any? {
        try? await controller.webView.evaluateJavaScript(script)
    }

    private func pause(_ seconds: Double) async {
        try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
    }

    func testARawPageRunsNoScriptLoadsNothingAndItsLinksAreAskedAbout() async throws {
        let probe = try Probe()
        await probe.start()
        let at = "http://127.0.0.1:\(probe.port)"
        let controller = ReportWebController(rules: try await ReportLockdown.ruleList())
        var links: [URL] = []
        controller.onLink = { links.append($0) }
        // Not sanitized: the view alone has to hold.
        try await load(controller, """
        <html><head><link rel="stylesheet" href="\(at)/style.css">
        <style>@import url("\(at)/import.css"); body { background-image: url("\(at)/background.png") }</style></head>
        <body><h1 id="t">Hello</h1>
        <img src="\(at)/beacon.png"><iframe src="\(at)/frame.html"></iframe><video src="\(at)/v.mp4" autoplay></video>
        <a id="a" href="https://example.com/pr/1">PR</a> <a id="f" href="file:///etc/hosts">file</a> <a id="j" href="#t">top</a>
        <img id="d" src="data:image/gif;base64,R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==">
        <script>document.getElementById('t').textContent = 'SCRIPT RAN'; fetch('\(at)/fetch'); new WebSocket('ws://127.0.0.1:\(probe.port)/ws');</script>
        </body></html>
        """)
        await pause(1.5)
        let heading = await eval(controller, "document.getElementById('t').textContent") as? String
        XCTAssertEqual(heading, "Hello", "the page's script did not run")
        let inline = await eval(controller, "document.getElementById('d').naturalWidth") as? Int
        XCTAssertEqual(inline, 1, "inline pictures show")
        // A navigation the page did not get from a tap: cancelled, not a link.
        _ = await eval(controller, "location.href = '\(at)/navigate'")
        await pause(0.8)
        XCTAssertTrue(controller.cancelled.contains { $0.absoluteString.hasSuffix("/navigate") }, "cancelled: \(controller.cancelled)")
        // Tapped links: cancelled; web addresses go to the app (which asks),
        // files nowhere (WebKit does not even ask for them from this page).
        _ = await eval(controller, "document.getElementById('a').click()")
        _ = await eval(controller, "document.getElementById('f').click()")
        await pause(0.8)
        XCTAssertEqual(links.map(\.absoluteString), ["https://example.com/pr/1"])
        XCTAssertTrue(controller.cancelled.contains { $0.absoluteString == "https://example.com/pr/1" })
        // A jump within the page stays.
        _ = await eval(controller, "document.getElementById('j').click()")
        await pause(0.3)
        let still = await eval(controller, "document.getElementById('t') ? 'here' : 'gone'") as? String
        XCTAssertEqual(still, "here", "the page stayed")
        XCTAssertEqual(probe.connections, 0, "nothing reached the network")
        // The probe does count connections (else the test proves nothing).
        _ = try? await URLSession.shared.data(from: URL(string: "\(at)/control")!)
        await pause(0.3)
        XCTAssertGreaterThan(probe.connections, 0)
        probe.stop()
    }

    func testTheCoresPagesAreSanitizedAndKeepTheirOwnStyles() async throws {
        let html = reportPageHtml(format: .html, body: """
        <style>h1 { color: rgb(255, 0, 255) }</style><h1 id="t" onclick="x()">Brief</h1>
        <script>document.getElementById('t').textContent = 'SCRIPT RAN'</script><img id="i" src="http://127.0.0.1:9/b.png" alt="b">
        """)
        XCTAssertTrue(html.contains("Content-Security-Policy"))
        XCTAssertFalse(html.contains("<script"))
        let controller = ReportWebController(rules: try await ReportLockdown.ruleList())
        try await load(controller, html)
        let color = await eval(controller, "getComputedStyle(document.getElementById('t')).color") as? String
        XCTAssertEqual(color, "rgb(255, 0, 255)", "the report's own CSS")
        let scripts = await eval(controller, "document.querySelectorAll('script').length") as? Int
        XCTAssertEqual(scripts, 0)
        let source = await eval(controller, "document.getElementById('i').hasAttribute('src')") as? Bool
        XCTAssertEqual(source, false, "a picture from the network lost its address")
        let markdown = reportPageHtml(format: .markdown, body: "# Root cause\n\n| a | b |\n|---|---|\n| 1 | 2 |")
        try await load(controller, markdown)
        let cells = await eval(controller, "document.querySelectorAll('td').length") as? Int
        XCTAssertEqual(cells, 2, "Markdown tables")
    }

    func testOnlyWebAndMailLinksMayOpen() {
        XCTAssertTrue(reportLinkAllowed(url: "https://github.com/x/y/pull/1"))
        XCTAssertTrue(reportLinkAllowed(url: "mailto:me@example.com"))
        XCTAssertFalse(reportLinkAllowed(url: "javascript:alert(1)"))
        XCTAssertFalse(reportLinkAllowed(url: "file:///etc/hosts"))
        XCTAssertFalse(reportLinkAllowed(url: "ccanvas://pair?code=x"))
    }
}
