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
        await store.send("   ")
        XCTAssertEqual(fake.sent.map(\.conversationId), ["c:a"])
        XCTAssertEqual(fake.sent.first?.body, "hello")
        XCTAssertEqual(fake.sent.first?.quote, Quote(author: "a", sentAt: 9, text: "orig", ranges: []))
        XCTAssertNil(store.replyTo)
        XCTAssertEqual(fake.sent.count, 1)
    }

    func testStylesGoWithTheTextAndWithACaption() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.select("c:a")
        let bold = BodyRange(start: 0, length: 2, style: .bold)
        await store.send("hi", ranges: [bold])
        store.addFiles([
            OutgoingFile(path: "/tmp/a.png", contentType: "image/png", width: nil, height: nil, isVoiceNote: false)
        ])
        await store.send("hi", ranges: [bold])
        XCTAssertEqual(fake.sentFiles.first?.caption, "hi")
        XCTAssertEqual(fake.sentRanges, [[bold], [bold]])
    }

    func testAGroupsMembersButUsCanBeMentionedByName() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        fake.members["g:00"] = ["alice-aci", "me-aci"]
        fake.titles["c:alice-aci"] = "Alice"
        let candidates = await store.mentionCandidates("g:00")
        XCTAssertEqual(candidates.map(\.0), ["alice-aci"])
        XCTAssertEqual(candidates.map(\.1), ["Alice"])
        let direct = await store.mentionCandidates("c:alice-aci")
        XCTAssertTrue(direct.isEmpty)
    }

    func testFindingAUsernameOpensItsChatOrSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.usernames["bob.7"] = "c:bob"
        let store = await linkedStore(fake)
        var unnamed = conversation("c:bob")
        unnamed.title = ""
        unnamed.username = "bob.7"
        XCTAssertEqual(AppStore.displayTitle(unnamed), "@bob.7", "until their name is known")
        let found = await store.findUsername("bob.7")
        XCTAssertNil(found)
        XCTAssertEqual(store.selectedId, "c:bob")
        let nobody = await store.findUsername("nobody.12")
        XCTAssertEqual(nobody, "No one has this username.")
        let bad = await store.findUsername("bad")
        XCTAssertEqual(bad, "a username looks like name.42")
    }

    func testChatsPicturesReachEveryAvatar() async {
        let fake = FakeClient(linked: true)
        var alice = conversation("c:alice")
        alice.avatar = "/data/avatar-c-alice"
        fake.conversationAnswers = [.success([alice, conversation("c:bob")])]
        let store = await linkedStore(fake)
        await eventually { AvatarPictures.shared.paths["c:alice"] != nil }
        XCTAssertEqual(AvatarPictures.shared.paths, ["c:alice": "/data/avatar-c-alice"])
        _ = store
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
        let calls = fake.titleCalls
        store.handle(.messageChanged(conversationId: "g:00", key: MessageKey(author: "carol", sentAt: 2)))
        try? await Task.sleep(for: .milliseconds(80))
        XCTAssertEqual(fake.titleCalls, calls)  // a name miss isn't asked again
    }

    func testABurstLargerThanAPageRestartsTheWindowWithoutAGap() {
        var hasOlder = false
        let loaded = (1...50).map { message("c:a", "a", Int64($0)) }
        let page = (200...299).map { message("c:a", "a", Int64($0)) }  // 150 newer arrived since
        let merged = AppStore.merge(loaded: loaded, newest: page, pageSize: 100, hasOlder: &hasOlder)
        XCTAssertEqual(merged.map(\.sentAt), Array(200...299).map(Int64.init))
        XCTAssertTrue(hasOlder)
    }

    func testReloadKeepsOlderPagesWhenTheyConnect() {
        var hasOlder = true
        let loaded = (1...150).map { message("c:a", "a", Int64($0)) }
        let page = (52...151).map { message("c:a", "a", Int64($0)) }
        let merged = AppStore.merge(loaded: loaded, newest: page, pageSize: 100, hasOlder: &hasOlder)
        XCTAssertEqual(merged.map(\.sentAt), Array(1...151).map(Int64.init))
    }

    func testAChangeToAnOlderLoadedMessageIsFetched() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...150).map { message("c:a", "a", Int64($0)) }
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.messages.count == 100 }
        await store.loadOlder()
        var all = fake.messageAnswers["c:a"]!
        all[4] = message("c:a", "a", 5, "edited")
        fake.messageAnswers["c:a"] = all
        fake.reactionAnswers[MessageKey(author: "a", sentAt: 5)] = [Reaction(reactor: "b", emoji: "👍")]
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 5)))
        await eventually { store.messages.first(where: { $0.sentAt == 5 })?.body == "edited" }
        await eventually { store.reactions[MessageKey(author: "a", sentAt: 5)]?.count == 1 }
    }

    func testOnlyNewOrChangedMessagesGetTheirReactionsFetched() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...10).map { message("c:a", "a", Int64($0)) }
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { fake.reactionCalls == 10 }
        fake.messageAnswers["c:a"] = (1...11).map { message("c:a", "a", Int64($0)) }
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 11)))
        await eventually { store.messages.count == 11 }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.reactionCalls, 11)
    }

    func testConcurrentLoadOlderAddsThePageOnce() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...250).map { message("c:a", "a", Int64($0)) }
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.messages.count == 100 }
        async let a: Void = store.loadOlder()
        async let b: Void = store.loadOlder()
        _ = await (a, b)
        XCTAssertEqual(store.messages.count, 200)
        XCTAssertEqual(Set(store.messages.map(\.sentAt)).count, 200)
    }

    func testTypingIsPerSenderAndClearsItself() async {
        let store = AppStore(open: { _ in FakeClient(linked: true) })
        store.handle(.typing(conversationId: "g:1", who: "a", started: true))
        store.handle(.typing(conversationId: "g:1", who: "b", started: true))
        store.handle(.typing(conversationId: "g:1", who: "a", started: false))
        XCTAssertTrue(store.isTyping("g:1"))  // b still typing
        store.handle(.typing(conversationId: "c:x", who: "x", started: true), now: .now.addingTimeInterval(-14.9))
        await eventually { !store.isTyping("c:x") }
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
