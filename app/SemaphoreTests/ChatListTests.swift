import XCTest

@testable import Semaphore

final class ChatListTests: XCTestCase {
    private func last(
        _ id: String, preview: String = "hi", author: String = "alice", outgoing: Bool = false,
        status: MessageStatus = .delivered, deleted: Bool = false
    ) -> Conversation {
        var c = conversation(id)
        c.lastMessagePreview = preview
        c.lastMessageAuthor = author
        c.lastMessageOutgoing = outgoing
        c.lastMessageStatus = status
        c.lastMessageDeleted = deleted
        return c
    }

    private func line(_ c: Conversation, selected: Bool = false, typing: Bool = false, draft: String = "")
        -> ChatList.Line
    {
        ChatList.line(c, selected: selected, typing: typing, draft: draft, authorName: "Alice")
    }

    func testDirectChatsShowTheTextAndTicksForOwnMessages() {
        XCTAssertEqual(line(last("c:a")), .message("hi", tick: nil))
        XCTAssertEqual(line(last("c:a", outgoing: true, status: .read)), .message("hi", tick: .doubleFilled))
        XCTAssertEqual(line(last("c:a", preview: "")), .message("Attachment", tick: nil))
    }

    func testGroupsNameTheAuthorLikeDesktop() {
        XCTAssertEqual(line(last("g:00")), .message("Alice: hi", tick: nil))
        XCTAssertEqual(line(last("g:00", outgoing: true, status: .pending)), .message("You: hi", tick: .clock))
    }

    func testDeletedLastMessageSaysWhoDeletedIt() {
        XCTAssertEqual(line(last("c:a", preview: "", deleted: true)), .message("Alice deleted this message", tick: nil))
        XCTAssertEqual(
            line(last("g:00", preview: "", outgoing: true, deleted: true)),
            .message("You deleted this message", tick: nil))
    }

    func testTypingThenDraftWinOverTheLastMessage() {
        XCTAssertEqual(line(last("c:a"), typing: true, draft: "unsent"), .typing)
        XCTAssertEqual(line(last("c:a"), draft: "unsent\nmore"), .draft("unsent more"))
        // the open chat shows its draft in the composer, not in the list
        XCTAssertEqual(line(last("c:a"), selected: true, draft: "unsent"), .message("hi", tick: nil))
        var empty = last("c:a")
        empty.lastMessageAt = 0
        XCTAssertEqual(line(empty), .message("", tick: nil))
    }

    func testNextAndPreviousWrapButUnreadDoesNotLikeDesktop() {
        let list = [
            conversation("c:1"), conversation("c:2", unread: 3), conversation("c:3"), conversation("c:4", unread: 1),
        ]
        func go(_ from: String?, _ step: Int, unread: Bool = false) -> String? {
            ChatList.neighbor(in: list, of: from, step: step, unreadOnly: unread)
        }
        XCTAssertEqual(go("c:1", 1), "c:2")
        XCTAssertEqual(go("c:4", 1), "c:1")
        XCTAssertEqual(go("c:1", -1), "c:4")
        XCTAssertEqual(go(nil, 1), "c:1")
        XCTAssertEqual(go(nil, -1), "c:4")
        XCTAssertEqual(go("c:1", 1, unread: true), "c:2")
        XCTAssertEqual(go("c:2", 1, unread: true), "c:4")
        XCTAssertEqual(go("c:4", 1, unread: true), nil)
        XCTAssertEqual(go("c:4", -1, unread: true), "c:2")
        XCTAssertEqual(go("c:2", -1, unread: true), nil)
        XCTAssertEqual(go(nil, -1, unread: true), "c:4")
        XCTAssertNil(ChatList.neighbor(in: [], of: nil, step: 1, unreadOnly: false))
        XCTAssertEqual(ChatList.at(2, in: list), "c:2")
        XCTAssertNil(ChatList.at(9, in: list))
        XCTAssertNil(ChatList.at(0, in: list))
    }

    func testDockBadgeIsTheTotalUnread() {
        XCTAssertNil(ChatList.badge([conversation("c:1")]))
        XCTAssertEqual(ChatList.badge([conversation("c:1", unread: 2), conversation("c:2", unread: 5)]), "7")
    }

    func testMarkReadOnlyWhenTheNewestMessageIsSeen() {
        XCTAssertTrue(ChatList.shouldMarkRead(atBottom: true, windowKey: true, unread: 2))
        XCTAssertFalse(ChatList.shouldMarkRead(atBottom: false, windowKey: true, unread: 2))
        XCTAssertFalse(ChatList.shouldMarkRead(atBottom: true, windowKey: false, unread: 2))
        XCTAssertFalse(ChatList.shouldMarkRead(atBottom: true, windowKey: true, unread: 0))
    }

    func testOwnChatIsNoteToSelf() {
        XCTAssertEqual(AppStore.displayTitle(conversation("c:me"), ownAci: "me"), "Note to Self")
        XCTAssertEqual(AppStore.displayTitle(conversation("c:other"), ownAci: "me"), "c:other")
    }
}

@MainActor
final class AppStoreChatListTests: XCTestCase {
    func testMarkReadAsksTheCoreOnceWhileUnread() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a", unread: 2)])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        store.markRead("c:a")
        store.markRead("c:a")  // the first call is still in flight
        await eventually { fake.markedRead == ["c:a"] }
        fake.conversationAnswers = [.success([conversation("c:a")])]
        store.handle(.conversationsChanged)
        await eventually { store.conversations.first?.unreadCount == 0 }
        store.markRead("c:a")  // nothing unread
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.markedRead, ["c:a"])
    }

    func testDockBadgeFollowsTheList() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a", unread: 2), conversation("c:b", unread: 1)])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.badge == "3" }
    }

    func testGroupListAuthorsGetTheirNames() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:a"] = "Alice"
        fake.conversationAnswers = [.success([conversation("g:00")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.senderName("a") == "Alice" }
    }
}
