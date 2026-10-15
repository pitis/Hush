import XCTest

@testable import Semaphore

@MainActor
final class AppStoreConversationTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    func testSelectingAConversationLoadsItsNewestMessages() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...150).map { message("c:a", "a", Int64($0)) }
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.messages.count == AppStore.pageSize }
        XCTAssertEqual(store.messages.last?.sentAt, 150)
        XCTAssertEqual(store.selectedId, "c:a")
    }

    func testLoadOlderPrependsThePreviousPage() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...150).map { message("c:a", "a", Int64($0)) }
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.messages.count == 100 }
        await store.loadOlder()
        XCTAssertEqual(store.messages.count, 150)
        XCTAssertEqual(store.messages.first?.sentAt, 1)
        XCTAssertFalse(store.hasOlder)
        await store.loadOlder()  // nothing more
        XCTAssertEqual(store.messages.count, 150)
    }

    func testANewMessageInTheOpenChatRefreshesItButOtherChatsDoNot() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = [message("c:a", "a", 1)]
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.messages.count == 1 }
        let calls = fake.messageCalls.count
        store.handle(.messageChanged(conversationId: "c:b", key: MessageKey(author: "b", sentAt: 2)))
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.messageCalls.count, calls)
        fake.messageAnswers["c:a"] = [message("c:a", "a", 1), message("c:a", "a", 3, "new")]
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 3)))
        await eventually { store.messages.last?.body == "new" }
    }

    func testSwitchingChatsNeverShowsThePreviousChatsMessages() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:slow"] = [message("c:slow", "s", 1, "slow")]
        fake.messageAnswers["c:fast"] = [message("c:fast", "f", 1, "fast")]
        fake.holdMessagesFor = "c:slow"
        let store = await linkedStore(fake)
        store.select("c:slow")
        await eventually { fake.isHoldingMessages }
        store.select("c:fast")
        await eventually { store.messages.first?.body == "fast" }
        fake.releaseMessages()
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(store.messages.map(\.body), ["fast"])
    }

    func testSendGoesToTheOpenChatAndClearsTheReply() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.select("c:a")
        let quoted = message("c:a", "a", 9, "orig")
        store.replyTo = quoted
        await store.send("  hello  ")
        XCTAssertEqual(fake.sent.map(\.conversationId), ["c:a"])
        XCTAssertEqual(fake.sent.first?.body, "hello")
        XCTAssertEqual(fake.sent.first?.quote, Quote(author: "a", sentAt: 9, text: "orig"))
        XCTAssertNil(store.replyTo)
        await store.send("   ")
        XCTAssertEqual(fake.sent.count, 1)
    }

    func testRetryAsksTheCore() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        await store.retry(MessageKey(author: "me-aci", sentAt: 4))
        XCTAssertEqual(fake.retried, [MessageKey(author: "me-aci", sentAt: 4)])
    }

    func testGroupSendersAreNamedFromContactTitles() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:bob"] = "Bob"
        fake.messageAnswers["g:00"] = [message("g:00", "bob", 1), message("g:00", "carol", 2)]
        let store = await linkedStore(fake)
        store.select("g:00")
        await eventually { store.senderName("bob") == "Bob" }
        XCTAssertEqual(store.senderName("carol"), "carol")  // unknown: short id, never empty
    }

    func testTypingShowsForFifteenSecondsUnlessStopped() async {
        let store = AppStore(open: { _ in FakeClient(linked: true) })
        let t0 = Date(timeIntervalSince1970: 1000)
        store.handle(.typing(conversationId: "c:a", who: "a", started: true), now: t0)
        XCTAssertTrue(store.isTyping("c:a", now: t0.addingTimeInterval(14)))
        XCTAssertFalse(store.isTyping("c:a", now: t0.addingTimeInterval(16)))
        store.handle(.typing(conversationId: "c:a", who: "a", started: true), now: t0)
        store.handle(.typing(conversationId: "c:a", who: "a", started: false), now: t0)
        XCTAssertFalse(store.isTyping("c:a", now: t0))
    }
}
