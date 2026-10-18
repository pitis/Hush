import XCTest

@testable import Hush

@MainActor
final class AppStoreBackupTests: XCTestCase {
    func testABackupReachesTheCoreAndAFailureSaysWhy() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        let url = URL(fileURLWithPath: "/tmp/Hush.hushbackup")
        let done = await store.backUp(to: url, passphrase: "correct horse")
        XCTAssertNil(done)
        XCTAssertEqual(fake.backups.map(\.path), ["/tmp/Hush.hushbackup"])
        XCTAssertEqual(fake.backups.map(\.passphrase), ["correct horse"])
        fake.backUpError = CoreError.Storage(message: "disk full")
        let failed = await store.backUp(to: url, passphrase: "correct horse")
        XCTAssertNotNil(failed)
        XCTAssertTrue(failed?.hasPrefix("Couldn't back up") ?? false, failed ?? "")
    }

    func testARestoredBackupKeepsTheKeyAndShowsAFreshCode() async {
        let fake = FakeClient(linked: false)
        let keyForgotten = Counter()
        let store = AppStore(open: { _ in fake }, forgetKey: { keyForgotten.increment() })
        await store.boot()
        XCTAssertEqual(store.screen, .link)
        let failed = await store.restoreBackup(
            from: URL(fileURLWithPath: "/tmp/Hush.hushbackup"), passphrase: "correct horse")
        XCTAssertNil(failed)
        XCTAssertEqual(fake.restores, ["/tmp/Hush.hushbackup"])
        XCTAssertEqual(keyForgotten.value, 0, "the restored data is under this Mac's key")
        XCTAssertEqual(store.screen, .link)
        XCTAssertEqual(fake.linkCalls, ["Hush", "Hush"], "a new core asks for a new code")
        XCTAssertEqual(store.restoreNote, "Your backup is restored. Link this Mac to your phone to finish.")
    }

    func testAWrongPassphraseKeepsTheCodeOnScreen() async {
        let fake = FakeClient(linked: false)
        fake.restoreError = CoreError.InvalidInput(message: "the passphrase is wrong, or the backup is damaged")
        let store = AppStore(open: { _ in fake })
        await store.boot()
        let failed = await store.restoreBackup(
            from: URL(fileURLWithPath: "/tmp/Hush.hushbackup"), passphrase: "battery staple")
        XCTAssertEqual(failed, "the passphrase is wrong, or the backup is damaged")
        XCTAssertEqual(fake.linkCalls, ["Hush"], "the core kept running")
        XCTAssertNil(store.restoreNote)
    }

    func testAFailureAfterTheCoreStoppedStartsOverAndSaysWhy() async {
        let fake = FakeClient(linked: false)
        fake.restoreError = CoreError.Storage(message: "disk full")
        let store = AppStore(open: { _ in fake })
        await store.boot()
        let failed = await store.restoreBackup(
            from: URL(fileURLWithPath: "/tmp/Hush.hushbackup"), passphrase: "correct horse")
        XCTAssertNil(failed, "the sheet went with the old screen: the new one says why")
        XCTAssertEqual(fake.linkCalls, ["Hush", "Hush"])
        store.handle(.linkUrl(url: "sgnl://linkdevice?x"))
        XCTAssertEqual(store.restoreNote, "Couldn't restore the backup: disk full", "the new code doesn't hide it")
    }
}
