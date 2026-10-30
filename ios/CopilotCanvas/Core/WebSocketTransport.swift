import Foundation

/// A WebSocket to the service (Dev Tunnels web forwarding, or the mock's
/// `ws://` address). Binary messages carry the protocol's byte stream; the
/// Rust core frames it.
final class WebSocketTransport: NSObject, Transport, URLSessionWebSocketDelegate, @unchecked Sendable {
    private var task: URLSessionWebSocketTask!
    private var session: URLSession!
    private let onData: (Data) -> Void
    private let onOpen: () -> Void
    private let onClose: (String) -> Void
    private let lock = NSLock()
    private var finished = false

    /// `token`: a Dev Tunnels connect token for a private tunnel (empty: none).
    init(url: URL, token: String, onOpen: @escaping () -> Void, onData: @escaping (Data) -> Void, onClose: @escaping (String) -> Void) {
        self.onData = onData
        self.onOpen = onOpen
        self.onClose = onClose
        super.init()
        let config = URLSessionConfiguration.default
        config.timeoutIntervalForRequest = 20
        config.waitsForConnectivity = false
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        session = URLSession(configuration: config, delegate: self, delegateQueue: queue)
        var request = URLRequest(url: url)
        request.timeoutInterval = 20
        if !token.isEmpty {
            request.setValue("tunnel \(token)", forHTTPHeaderField: "X-Tunnel-Authorization")
        }
        request.setValue("true", forHTTPHeaderField: "X-Tunnel-Skip-AntiPhishing-Page")
        task = session.webSocketTask(with: request)
        // Frames are at most 32 MiB; the default limit is 1 MiB.
        task.maximumMessageSize = 64 * 1024 * 1024
    }

    func start() {
        task.resume()
        receive()
    }

    private func receive() {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(.data(let data)):
                self.onData(data)
                self.receive()
            case .success(.string(let text)):
                self.onData(Data(text.utf8))
                self.receive()
            case .success:
                self.receive()
            case .failure(let error):
                self.finish(self.describe(error))
            }
        }
    }

    /// Why the connection failed, in words for the banner.
    private func describe(_ error: Error) -> String {
        if let response = task.response as? HTTPURLResponse {
            switch response.statusCode {
            case 401, 403:
                return "The tunnel refused access (HTTP \(response.statusCode)): the access token expired or is missing. Scan a new QR code (`copilot-canvas remote pair --qr` on the Cloud PC)."
            case 302, 307:
                return "The tunnel asks for a sign-in: this phone has no valid tunnel access token. Scan a new QR code on the Cloud PC."
            case 404, 502, 503:
                return "The tunnel is not hosting the service right now (HTTP \(response.statusCode)): is the Cloud PC on and the tunnel automation running?"
            default:
                return "HTTP \(response.statusCode): \(error.localizedDescription)"
            }
        }
        return error.localizedDescription
    }

    private func finish(_ reason: String) {
        lock.lock()
        let first = !finished
        finished = true
        lock.unlock()
        guard first else { return }
        task.cancel(with: .goingAway, reason: nil)
        session.finishTasksAndInvalidate()
        onClose(reason)
    }

    // MARK: Transport (called by the Rust core)

    func send(data: Data) {
        task.send(.data(data)) { [weak self] error in
            if let error, let self { self.finish(self.describe(error)) }
        }
    }

    func close() {
        finish("closed")
    }

    // MARK: URLSessionWebSocketDelegate

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask, didOpenWithProtocol protocol: String?) {
        onOpen()
    }

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask, didCloseWith closeCode: URLSessionWebSocketTask.CloseCode, reason: Data?) {
        finish("the service closed the connection")
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        finish(error.map { describe($0) } ?? "the connection ended")
    }
}
