import SwiftUI
import WebKit

/// How every report page is shown: a web view that runs no script from the
/// page, loads nothing from the network and goes nowhere. On top of the
/// core's sanitizer and the page's Content-Security-Policy (`report_html`),
/// like the desktop GUI's WebView2 view:
///
/// - JavaScript from the page is off (`allowsContentJavaScript`);
/// - a content rule list blocks every load with an address (`http(s)`,
///   `ws(s)`, `ftp`, `file`, `blob`): the page itself arrives as a string,
///   pictures only as `data:` URIs;
/// - every navigation but the page's own load (and jumps within it) is
///   cancelled; a link you tap becomes a request to the app, which asks
///   before Safari opens it; no new windows, no link previews (they would
///   load the page), no data detectors, no media;
/// - its own non-persistent data store (no cookies, cache or storage kept).
enum ReportLockdown {
    /// Loads with an address are blocked; `data:` and the page's own
    /// `about:blank` are not.
    static let rules = """
    [
      {"trigger": {"url-filter": "^https?:"}, "action": {"type": "block"}},
      {"trigger": {"url-filter": "^wss?:"}, "action": {"type": "block"}},
      {"trigger": {"url-filter": "^ftp:"}, "action": {"type": "block"}},
      {"trigger": {"url-filter": "^file:"}, "action": {"type": "block"}},
      {"trigger": {"url-filter": "^blob:"}, "action": {"type": "block"}}
    ]
    """

    @MainActor private static var compiled: WKContentRuleList?

    /// The compiled rule list (once per launch). A failure means no report is
    /// shown: the view never opens without it.
    @MainActor
    static func ruleList() async throws -> WKContentRuleList {
        if let compiled { return compiled }
        guard let store = WKContentRuleListStore.default() else { throw LockdownError.noStore }
        let list = try await store.compileContentRuleList(forIdentifier: "copilot-canvas-reports-v1", encodedContentRuleList: rules)
        guard let list else { throw LockdownError.noStore }
        compiled = list
        return list
    }

    enum LockdownError: LocalizedError {
        case noStore
        var errorDescription: String? { "The report view cannot block network loads on this device." }
    }

    @MainActor
    static func configuration(rules: WKContentRuleList) -> WKWebViewConfiguration {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isFraudulentWebsiteWarningEnabled = false
        config.allowsInlineMediaPlayback = false
        config.allowsAirPlayForMediaPlayback = false
        config.allowsPictureInPictureMediaPlayback = false
        config.mediaTypesRequiringUserActionForPlayback = .all
        config.dataDetectorTypes = []
        config.userContentController.add(rules)
        return config
    }
}

/// One report page in a locked-down web view (also note pages and todo
/// bodies); tapped links go to `onLink`, links into the app to `onAppLink`.
@MainActor
final class ReportWebController: NSObject, WKNavigationDelegate, WKUIDelegate {
    let webView: WKWebView
    /// A link was tapped (an `http(s)` or `mailto` address): ask, then open.
    var onLink: ((URL) -> Void)?
    /// A link into the app was tapped: `#note:Name` or `#todo:DEC-031` (note pages, todo bodies).
    var onAppLink: ((NoteLink) -> Void)?
    /// The page finished loading (tests).
    var onFinish: (() -> Void)?
    /// Navigations cancelled (tests).
    private(set) var cancelled: [URL] = []
    /// The page's own load, still to come.
    private var expecting = false

    init(rules: WKContentRuleList, identifier: String = "reportPage") {
        webView = WKWebView(frame: .zero, configuration: ReportLockdown.configuration(rules: rules))
        super.init()
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.allowsLinkPreview = false
        webView.allowsBackForwardNavigationGestures = false
        webView.isOpaque = false
        webView.backgroundColor = TerminalColors.ui(TerminalColors.background)
        webView.scrollView.backgroundColor = TerminalColors.ui(TerminalColors.background)
        webView.accessibilityIdentifier = identifier
    }

    /// Shows a page (the core's sanitized document; tests also load raw HTML).
    func load(_ html: String) {
        expecting = true
        webView.loadHTMLString(Self.appLinks(html), baseURL: nil)
    }

    /// The scheme in-app links get: WebKit never asks about jumps within a
    /// page (`#note:…`), but it does about another scheme (always cancelled).
    static let appScheme = "canvas-app"

    /// `href="#note:…"` / `href="#todo:…"` as `canvas-app:note:…` links.
    static func appLinks(_ html: String) -> String {
        var out = html
        for quote in ["\"", "'"] {
            for kind in ["note", "todo"] {
                out = out.replacingOccurrences(of: "href=\(quote)#\(kind):", with: "href=\(quote)\(appScheme):\(kind):")
            }
        }
        return out
    }

    /// The page's text as shown (Copy, Share).
    func text() async -> String {
        (try? await webView.evaluateJavaScript("document.body ? document.body.innerText : ''") as? String) ?? ""
    }

    /// The page's height in points (a body shown inside a list).
    func contentHeight() async -> CGFloat {
        let height = (try? await webView.evaluateJavaScript("document.documentElement.scrollHeight") as? Double) ?? 0
        return CGFloat(height)
    }

    /// What an in-app link opens (`canvas-app:note:…`, `#note:` / `#todo:`; nil: another link).
    static func appLink(_ address: String) -> NoteLink? {
        if address.hasPrefix("\(appScheme):") {
            return noteLinkTarget(href: "#" + address.dropFirst(appScheme.count + 1))
        }
        guard let hash = address.firstIndex(of: "#") else { return nil }
        return noteLinkTarget(href: String(address[hash...]))
    }

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void) {
        let url = action.request.url
        let address = url?.absoluteString ?? ""
        // The page itself (`loadHTMLString` without a base: about:blank), once.
        if expecting, action.navigationType == .other, address == "about:blank", action.targetFrame?.isMainFrame == true {
            expecting = false
            decisionHandler(.allow)
            return
        }
        // A link into the app (`#note:Name`, `#todo:DEC-031`): never a navigation.
        if url?.scheme == Self.appScheme || address.hasPrefix("about:blank#"), let target = Self.appLink(address) {
            decisionHandler(.cancel)
            if action.navigationType == .linkActivated { onAppLink?(target) }
            return
        }
        // A jump within the page (a table of contents).
        if address.hasPrefix("about:blank#"), action.targetFrame?.isMainFrame == true {
            decisionHandler(.allow)
            return
        }
        decisionHandler(.cancel)
        if let url { cancelled.append(url) }
        if action.navigationType == .linkActivated, let url, reportLinkAllowed(url: address) {
            onLink?(url)
        }
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        onFinish?()
    }

    /// `target=_blank` and `window.open`: no new window; a tapped link is asked about.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if action.navigationType == .linkActivated, let url = action.request.url, reportLinkAllowed(url: url.absoluteString) {
            onLink?(url)
        }
        return nil
    }

    /// No camera, microphone or other device for a page.
    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin, initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType, decisionHandler: @escaping @MainActor @Sendable (WKPermissionDecision) -> Void) {
        decisionHandler(.deny)
    }
}

/// The controller's web view in SwiftUI.
struct ReportWebView: UIViewRepresentable {
    let controller: ReportWebController

    func makeUIView(context: Context) -> WKWebView { controller.webView }
    func updateUIView(_ view: WKWebView, context: Context) {}
}
