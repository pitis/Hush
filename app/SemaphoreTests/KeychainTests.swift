import XCTest

@testable import Semaphore

final class KeychainTests: XCTestCase {
    func testKeyIsCreatedOnceAndIs64Hex() throws {
        let keychain = Keychain(service: "dev.pitis.semaphore.tests", account: "db-key-\(UUID())")
        defer { keychain.delete() }
        let first = try keychain.dbKey()
        let second = try keychain.dbKey()
        XCTAssertEqual(first, second)
        XCTAssertEqual(first.count, 64)
        XCTAssertTrue(first.allSatisfy { $0.isHexDigit })
    }

    func testDifferentAccountsGetDifferentKeys() throws {
        let a = Keychain(service: "dev.pitis.semaphore.tests", account: "a-\(UUID())")
        let b = Keychain(service: "dev.pitis.semaphore.tests", account: "b-\(UUID())")
        defer {
            a.delete()
            b.delete()
        }
        XCTAssertNotEqual(try a.dbKey(), try b.dbKey())
    }
}
