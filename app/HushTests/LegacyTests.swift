import XCTest

@testable import Hush

/// The app was renamed from Semaphore: its data, key and settings move over once.
final class LegacyTests: XCTestCase {
    func testOldNamesNeverChange() {
        // they name where the data of every install made before the rename is
        XCTAssertEqual(Legacy.name, "Semaphore")
        XCTAssertEqual(Legacy.bundleId, "dev.pitis.semaphore")
        XCTAssertEqual(Keychain().legacyService, "dev.pitis.semaphore")
    }

    func testKeyUnderTheOldServiceIsMovedOnce() throws {
        let account = "moved-\(UUID())"
        let old = Keychain(service: "dev.pitis.hush.tests.old", account: account, legacyService: nil)
        let new = Keychain(service: "dev.pitis.hush.tests", account: account, legacyService: old.service)
        defer {
            old.delete()
            new.delete()
        }
        let key = try old.dbKey()
        XCTAssertEqual(try new.dbKey(), key)
        // the old item is gone: reading it again would create a fresh key
        XCTAssertNotEqual(try old.dbKey(), key)
        XCTAssertEqual(try new.dbKey(), key)
    }

    func testUnreadableOldKeyThrowsInsteadOfMakingANewOne() throws {
        let account = "bad-old-\(UUID())"
        let new = Keychain(service: "dev.pitis.hush.tests", account: account, legacyService: "dev.pitis.hush.tests.old")
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: "dev.pitis.hush.tests.old",
            kSecAttrAccount as String: account, kSecValueData as String: Data([0xff, 0xfe]),
        ]
        XCTAssertEqual(SecItemAdd(item as CFDictionary, nil), errSecSuccess)
        defer {
            SecItemDelete(item as CFDictionary)
            new.delete()
        }
        XCTAssertThrowsError(try new.dbKey())
    }

    func testOldDataFolderIsMovedWhenThereIsNoNewOne() throws {
        let support = try temporaryFolder()
        let old = support.appendingPathComponent("Semaphore")
        try FileManager.default.createDirectory(at: old, withIntermediateDirectories: true)
        try Data("db".utf8).write(to: old.appendingPathComponent("projection.db"))

        let dir = try AppStore.dataDirectory(in: support)
        XCTAssertEqual(dir.lastPathComponent, "Hush")
        XCTAssertEqual(try Data(contentsOf: dir.appendingPathComponent("projection.db")), Data("db".utf8))
        XCTAssertFalse(FileManager.default.fileExists(atPath: old.path))
    }

    func testExistingNewDataFolderIsKept() throws {
        let support = try temporaryFolder()
        for name in ["Semaphore", "Hush"] {
            let dir = support.appendingPathComponent(name)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try Data(name.utf8).write(to: dir.appendingPathComponent("projection.db"))
        }
        let dir = try AppStore.dataDirectory(in: support)
        XCTAssertEqual(try Data(contentsOf: dir.appendingPathComponent("projection.db")), Data("Hush".utf8))
        XCTAssertTrue(FileManager.default.fileExists(atPath: support.appendingPathComponent("Semaphore").path))
    }

    func testSettingsAreAdoptedWithoutOverwritingNewOnes() throws {
        let suite = "dev.pitis.hush.tests.\(UUID())"
        let legacy = "dev.pitis.hush.tests.legacy.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer {
            defaults.removePersistentDomain(forName: suite)
            defaults.removePersistentDomain(forName: legacy)
        }
        defaults.setPersistentDomain(["appearance": "light", "showInMenuBar": false], forName: legacy)
        defaults.set("dark", forKey: "appearance")

        Legacy.adoptSettings(into: defaults, from: legacy)
        XCTAssertEqual(defaults.string(forKey: "appearance"), "dark")
        XCTAssertEqual(defaults.object(forKey: "showInMenuBar") as? Bool, false)
        XCTAssertNil(defaults.persistentDomain(forName: legacy))
    }

    private func temporaryFolder() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("legacy-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }
}
