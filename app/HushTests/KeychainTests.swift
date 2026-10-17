import XCTest

@testable import Hush

final class KeychainTests: XCTestCase {
    func testKeyIsCreatedOnceAndIs64Hex() throws {
        let keychain = Keychain(service: "dev.pitis.hush.tests", account: "db-key-\(UUID())")
        defer { keychain.delete() }
        let first = try keychain.dbKey()
        let second = try keychain.dbKey()
        XCTAssertEqual(first, second)
        XCTAssertEqual(first.count, 64)
        XCTAssertTrue(first.allSatisfy { $0.isHexDigit })
    }

    func testDifferentAccountsGetDifferentKeys() throws {
        let a = Keychain(service: "dev.pitis.hush.tests", account: "a-\(UUID())")
        let b = Keychain(service: "dev.pitis.hush.tests", account: "b-\(UUID())")
        defer {
            a.delete()
            b.delete()
        }
        XCTAssertNotEqual(try a.dbKey(), try b.dbKey())
    }
}

final class KeychainSafetyTests: XCTestCase {
    func testDefaultsNeverChange() {
        // renaming these would silently create a new key and make the existing data unreadable
        XCTAssertEqual(Keychain().service, "dev.pitis.hush")
        XCTAssertEqual(Keychain().account, "db-key")
    }

    func testUnreadableExistingItemThrowsInsteadOfBeingReplaced() throws {
        let keychain = Keychain(service: "dev.pitis.hush.tests", account: "bad-\(UUID())")
        defer { keychain.delete() }
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: keychain.service,
            kSecAttrAccount as String: keychain.account, kSecValueData as String: Data([0xff, 0xfe]),
        ]
        XCTAssertEqual(SecItemAdd(item as CFDictionary, nil), errSecSuccess)
        XCTAssertThrowsError(try keychain.dbKey())
        var query = item
        query.removeValue(forKey: kSecValueData as String)
        query[kSecReturnData as String] = true
        var out: CFTypeRef?
        XCTAssertEqual(SecItemCopyMatching(query as CFDictionary, &out), errSecSuccess)
        XCTAssertEqual(out as? Data, Data([0xff, 0xfe]))
    }
}
