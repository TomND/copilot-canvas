import Foundation
import Security

/// A service this phone paired with: where it is, its id there and its secret.
struct PairedServer: Codable, Identifiable, Hashable {
    var id: UUID = UUID()
    /// What the phone calls it ("cloudpc").
    var name: String
    /// `wss://…devtunnels.ms` (or `ws://HOST:PORT` for the mock service).
    var url: String
    /// The id the service gave this phone (`pc-…`).
    var clientId: String
    /// The phone's 32-byte secret.
    var secret: Data
    /// The newest Dev Tunnels connect token (empty: none needed).
    var token: String = ""
    /// When the token expires (ms since 1970; 0: unknown).
    var tokenExpiresMs: UInt64 = 0
    var addedMs: UInt64 = UInt64(Date().timeIntervalSince1970 * 1000)

    var tokenExpired: Bool {
        !token.isEmpty && tokenExpiresMs > 0 && tokenExpiresMs < UInt64(Date().timeIntervalSince1970 * 1000)
    }

    var isTunnel: Bool { url.contains(".devtunnels.ms") }
}

/// Paired services, kept in the Keychain (this device only, after the first
/// unlock): a paired phone can do what you can on the Cloud PC.
final class ServerStore {
    static let shared = ServerStore()
    private let service = "com.thomashorga.copilotcanvas.servers"
    private let account = "paired"

    func load() -> [PairedServer] {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess, let data = item as? Data else { return [] }
        return (try? JSONDecoder().decode([PairedServer].self, from: data)) ?? []
    }

    /// Saves the list; `nil`, or why the Keychain refused it.
    @discardableResult
    func save(_ servers: [PairedServer]) -> String? {
        guard let data = try? JSONEncoder().encode(servers) else { return "cannot encode the paired services" }
        let update: [String: Any] = [kSecValueData as String: data]
        var status = SecItemUpdate(baseQuery() as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var add = baseQuery()
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            status = SecItemAdd(add as CFDictionary, nil)
        }
        guard status == errSecSuccess else {
            let reason = SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)"
            return "The pairing could not be saved in the Keychain (\(reason)); it lasts until the app quits."
        }
        return nil
    }

    private func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }
}
