import Foundation
import Security

/// The database key, created once per Mac and kept in the login keychain.
struct Keychain {
    var service = "dev.pitis.semaphore"
    var account = "db-key"

    /// 32 random bytes as 64 lowercase hex characters; created on first use.
    func dbKey() throws -> String {
        if let existing = try read() { return existing }
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw KeychainError.random
        }
        let hex = bytes.map { String(format: "%02x", $0) }.joined()
        var item = baseQuery
        item[kSecValueData as String] = Data(hex.utf8)
        // The login keychain ties access to the app's code signature (see app/project.yml).
        let status = SecItemAdd(item as CFDictionary, nil)
        if status == errSecDuplicateItem, let winner = try read() { return winner }  // created concurrently
        guard status == errSecSuccess else { throw KeychainError.status(status) }
        return hex
    }

    func delete() {
        SecItemDelete(baseQuery as CFDictionary)
    }

    private var baseQuery: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private func read() throws -> String? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        var out: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &out)
        if status == errSecItemNotFound { return nil }
        // Any other failure must not fall through to creating a new key: that would make the
        // existing (encrypted) data unreadable.
        guard status == errSecSuccess, let data = out as? Data, let hex = String(data: data, encoding: .utf8) else {
            throw KeychainError.status(status)
        }
        return hex
    }
}

enum KeychainError: Error {
    case random
    case status(OSStatus)
}
