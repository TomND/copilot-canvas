import Foundation
import UIKit

struct PairingFailed: LocalizedError {
    let reason: String
    var errorDescription: String? { reason }
}

/// Pairs this phone with a service: a fresh WebSocket that opens with the
/// pairing exchange (the service proves it knows the code first).
enum Pairer {
    private final class Box: @unchecked Sendable {
        let lock = NSLock()
        var pairing: Pairing?
        weak var transport: WebSocketTransport?
        var done = false
        var continuation: CheckedContinuation<PairResult, Error>?

        func finish(_ result: Result<PairResult, Error>) {
            lock.lock()
            let continuation = done ? nil : self.continuation
            done = true
            lock.unlock()
            continuation?.resume(with: result)
        }
    }

    @MainActor
    static func pair(link: PairLinkInfo) async throws -> PairedServer {
        guard let url = URL(string: link.url) else { throw PairingFailed(reason: "Invalid address \(link.url)") }
        let phone = UIDevice.current.name.filter { $0.isLetter || $0.isNumber || $0 == "-" }.prefix(30)
        let name = "iphone-\(phone.isEmpty ? "phone" : String(phone))"
        let box = Box()
        var transport: WebSocketTransport?
        let result: PairResult = try await withCheckedThrowingContinuation { continuation in
            box.continuation = continuation
            let made = WebSocketTransport(
                url: url,
                token: link.token,
                onOpen: {
                    guard let transport = box.transport else { return }
                    let pairing = Pairing(transport: transport)
                    box.lock.lock()
                    box.pairing = pairing
                    box.lock.unlock()
                    DispatchQueue.global(qos: .userInitiated).async {
                        do {
                            box.finish(.success(try pairing.run(code: link.code, name: name)))
                        } catch {
                            box.finish(.failure(error))
                        }
                    }
                },
                onData: { data in
                    box.lock.lock()
                    let pairing = box.pairing
                    box.lock.unlock()
                    pairing?.receive(data: data)
                },
                onClose: { reason in
                    box.lock.lock()
                    let pairing = box.pairing
                    box.lock.unlock()
                    pairing?.transportClosed()
                    if pairing == nil { box.finish(.failure(PairingFailed(reason: reason))) }
                }
            )
            box.transport = made
            transport = made
            made.start()
        }
        transport?.close()
        return PairedServer(name: link.name.isEmpty ? "cloudpc" : link.name, url: link.url, clientId: result.clientId, secret: result.secret, token: link.token, tokenExpiresMs: link.tokenExpiresMs)
    }
}
