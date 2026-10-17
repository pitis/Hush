import Foundation
import Security

/// The database key, created once per Mac and kept in the login keychain.
struct Keychain {
    var service = "dev.pitis.hush"
    var account = "db-key"
    /// The service the key was kept under before the app was renamed from Semaphore; a key found
    /// there is moved to `service` once.
    var legacyService: String? = Legacy.bundleId

    /// 32 random bytes as 64 lowercase hex characters; created on first use.
    func dbKey() throws -> String {
        if let existing = try read() { return existing }
        if let moved = try moveLegacy() { return moved }
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw KeychainError.random
        }
        return try add(bytes.map { String(format: "%02x", $0) }.joined())
    }

    func delete() {
        SecItemDelete(baseQuery as CFDictionary)
    }

    /// The old item is deleted only once its key is stored under the new service.
    private func moveLegacy() throws -> String? {
        guard let legacyService else { return nil }
        let legacy = Keychain(service: legacyService, account: account, legacyService: nil)
        guard let hex = try legacy.read() else { return nil }
        let stored = try add(hex)
        legacy.delete()
        return stored
    }

    private func add(_ hex: String) throws -> String {
        var item = baseQuery
        item[kSecValueData as String] = Data(hex.utf8)
        // The login keychain ties access to the app's code signature (see app/project.yml).
        let status = SecItemAdd(item as CFDictionary, nil)
        if status == errSecDuplicateItem, let winner = try read() { return winner }  // created concurrently
        guard status == errSecSuccess else { throw KeychainError.status(status) }
        return hex
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
