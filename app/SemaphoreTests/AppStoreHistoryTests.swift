import XCTest

@testable import Semaphore

@MainActor
final class AppStoreHistoryTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        XCTAssertEqual(store.screen, .chats)
        return store
    }

    func testProgressEventsDriveTheOverlay() async {
        let store = await linkedStore(FakeClient(linked: true))
        XCTAssertNil(store.history)
        store.handle(.historyProgress(stage: .waiting, done: 0, total: 0))
        XCTAssertEqual(store.history, .init(stage: .waiting, done: 0, total: 0))
        store.handle(.historyProgress(stage: .downloading, done: 512, total: 1024))
        XCTAssertEqual(store.history, .init(stage: .downloading, done: 512, total: 1024))
        store.handle(.historyProgress(stage: .importing, done: 2000, total: 0))
        XCTAssertEqual(store.history?.done, 2000)
        store.handle(.historyProgress(stage: .done, done: 2000, total: 0))
        XCTAssertNil(store.history, "done dismisses the overlay")
    }

    func testNoHistoryDismissesAndFailureStays() async {
        let store = await linkedStore(FakeClient(linked: true))
        store.handle(.historyProgress(stage: .waiting, done: 0, total: 0))
        store.handle(.historyProgress(stage: .noHistory, done: 0, total: 0))
        XCTAssertNil(store.history)
        store.handle(.historyProgress(stage: .downloading, done: 1, total: 2))
        store.handle(.historyProgress(stage: .failed, done: 0, total: 0))
        XCTAssertEqual(store.history?.stage, .failed, "failure waits for Retry or Continue")
    }

    func testBootShowsTheOverlayWhenAnImportIsPending() async {
        let fake = FakeClient(linked: true)
        fake.historyPendingAnswer = true
        let store = await linkedStore(fake)
        XCTAssertEqual(store.history, .init(stage: .waiting, done: 0, total: 0))
    }

    func testCancelAndRetryReachTheClient() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.handle(.historyProgress(stage: .failed, done: 0, total: 0))
        store.cancelHistory()
        XCTAssertEqual(fake.cancelHistoryCalls, 1)
        store.retryHistory()
        XCTAssertEqual(fake.retryHistoryCalls, 1)
        XCTAssertEqual(store.history?.stage, .waiting, "retry shows progress again at once")
    }

    func testRelinkKeepsTheKeyAndShowsTheCode() async {
        let fake = FakeClient(linked: true)
        let keyForgotten = Counter()
        let store = AppStore(open: { _ in fake }, forgetKey: { keyForgotten.increment() })
        await store.boot()
        XCTAssertEqual(store.screen, .chats)
        let failed = await store.relinkForHistory()
        XCTAssertNil(failed)
        XCTAssertEqual(fake.relinkCalls, 1)
        XCTAssertEqual(keyForgotten.value, 0, "the data stays, so its key stays")
        XCTAssertEqual(store.screen, .link, "back at the QR code")
        XCTAssertEqual(fake.linkCalls, ["Semaphore"])
    }

    func testRelinkFailureReportsAndStays() async {
        let fake = FakeClient(linked: true)
        fake.relinkError = CoreError.Network(message: "offline")
        let store = await linkedStore(fake)
        let failed = await store.relinkForHistory()
        XCTAssertNotNil(failed)
        XCTAssertEqual(store.screen, .chats, "nothing was torn down")
    }
}
