import XCTest

@testable import Hush

final class TimelineEventTests: XCTestCase {
    func testChatEventsSitAmongMessagesByTime() {
        let event = ChatEvent(id: 1, conversationId: "c:a", at: 15, kind: .decryptionFailed, author: "a", value: nil)
        let items = Timeline.items(for: [message("c:a", "a", 10), message("c:a", "a", 20)], events: [event])
        let ids = items.compactMap { item -> String? in
            if case .day = item { return nil }
            return item.id
        }
        XCTAssertEqual(ids, ["a-10", "event-1", "a-20"])
    }

    func testOnlyTheNetworkComingBackReconnects() {
        XCTAssertTrue(NetworkWatch.shouldReconnect(was: false, now: true))
        XCTAssertFalse(NetworkWatch.shouldReconnect(was: true, now: true))
        XCTAssertFalse(NetworkWatch.shouldReconnect(was: true, now: false))
        XCTAssertFalse(NetworkWatch.shouldReconnect(was: nil, now: true), "the first report isn't a change")
    }
}

@MainActor
final class AppStoreResilienceTests: XCTestCase {
    private let keysForgotten = Counter()
    private let opens = Counter()

    private func store(_ fake: FakeClient) async -> AppStore {
        let (opens, keysForgotten) = (opens, keysForgotten)
        let store = AppStore(
            open: { _ in
                opens.increment()
                return fake
            }, forgetKey: { keysForgotten.increment() })
        await store.boot()
        return store
    }

    func testBeingUnlinkedOnThePhoneShowsTheUnlinkedScreen() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        XCTAssertEqual(store.screen, .chats)
        fake.setLinked(false)
        store.handle(.connection(state: .unlinked))
        XCTAssertEqual(store.screen, .unlinked)
    }

    func testAFailedLinkingAttemptStaysOnTheLinkScreen() async {
        let fake = FakeClient(linked: false)
        let store = await store(fake)
        store.handle(.connection(state: .unlinked))
        XCTAssertEqual(store.screen, .link)
    }

    func testLinkingAgainWipesEverythingAndStartsOverWithoutARestart() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a")])]
        let store = await store(fake)
        await eventually { store.conversations.count == 1 }
        fake.setLinked(false)
        store.handle(.connection(state: .unlinked))
        await store.linkAgain()
        XCTAssertEqual(fake.unlinks, [false], "the phone already removed it")
        XCTAssertEqual(keysForgotten.value, 1)
        XCTAssertEqual(opens.value, 2, "a fresh core")
        XCTAssertEqual(store.screen, .link)
        XCTAssertTrue(store.conversations.isEmpty)
        XCTAssertEqual(fake.linkCalls.count, 1)
    }

    func testUnlinkingThisMacTellsTheServerOrSaysWhyNot() async {
        let fake = FakeClient(linked: true)
        fake.unlinkError = CoreError.Network(message: "offline")
        let store = await store(fake)
        let failed = await store.unlinkThisMac(tellServer: true)
        XCTAssertNotNil(failed)
        XCTAssertEqual(store.screen, .chats, "nothing removed")
        XCTAssertEqual(keysForgotten.value, 0)
        fake.unlinkError = nil
        let done = await store.unlinkThisMac(tellServer: false)  // "Unlink anyway"
        XCTAssertNil(done)
        XCTAssertEqual(fake.unlinks, [true, false])
        XCTAssertEqual(keysForgotten.value, 1)
        XCTAssertEqual(store.screen, .link)
    }

    func testErrorsAreKeptForTheCoreLog() async {
        let store = await store(FakeClient(linked: true))
        for i in 0..<(AppStore.logLimit + 5) { store.handle(.error(message: "e\(i)")) }
        XCTAssertEqual(store.errors.count, AppStore.logLimit)
        XCTAssertEqual(store.errors.last?.text, "e\(AppStore.logLimit + 4)")
    }

    func testRetryReconnectsAndUndecryptableMessagesShowWithAReset() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = [message("c:a", "a", 10)]
        let store = await store(fake)
        store.retryConnection()
        XCTAssertEqual(fake.reconnects, 1)
        store.select("c:a")
        await eventually { store.messages.count == 1 }
        fake.eventAnswers["c:a"] = [
            ChatEvent(id: 1, conversationId: "c:a", at: 20, kind: .decryptionFailed, author: "a", value: nil)
        ]
        store.handle(.chatEventAdded(conversationId: "c:a"))
        await eventually { store.events.count == 1 }
        XCTAssertTrue(store.timelineItems.contains { $0.id == "event-1" })
        store.resetSession("c:a")
        XCTAssertEqual(fake.sessionResets, ["c:a"])
    }

    func testAWipeThatFailsAfterTheCoreStoppedSaysSoInsteadOfHanging() async {
        let fake = FakeClient(linked: true)
        fake.unlinkError = CoreError.Storage(message: "permission denied")
        let store = await store(fake)
        let failed = await store.unlinkThisMac(tellServer: true)
        XCTAssertNil(failed, "not a network problem: no \"server couldn't be reached\" alert")
        guard case .failed(let reason) = store.screen else { return XCTFail("\(store.screen)") }
        XCTAssertTrue(reason.contains("permission denied"))
    }

    func testAnUnreachableServerIsWordedForPeople() async {
        let fake = FakeClient(linked: true)
        fake.unlinkError = CoreError.Network(message: "timed out")
        let store = await store(fake)
        let failed = await store.unlinkThisMac(tellServer: true)
        XCTAssertEqual(failed, "The server couldn't be reached: timed out")
    }

    func testTheOldCoreIsIgnoredAfterStartingOver() async {
        let fake = FakeClient(linked: true)
        let notifier = FakeNotifier()
        let store = AppStore(open: { _ in fake }, notifier: notifier, forgetKey: {})
        await store.boot()
        let old = store.listenerGeneration
        await store.unlinkThisMac(tellServer: false).map { XCTFail($0) }
        XCTAssertEqual(notifier.removedAll, 1, "message previews go with the data")
        store.receive(.error(message: "the connection to the server closed"), generation: old)
        XCTAssertNil(store.lastError, "a late event of the old core changes nothing")
        store.receive(.error(message: "now"), generation: store.listenerGeneration)
        XCTAssertEqual(store.lastError, "now")
    }

    func testRetryRestartsReceivingIfItStopped() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        let starts = fake.startCalls
        store.retryConnection()
        XCTAssertEqual(fake.startCalls, starts + 1)
        XCTAssertEqual(fake.reconnects, 1)
    }
}
