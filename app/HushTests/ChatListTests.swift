import XCTest

@testable import Hush

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

    func testRequestsAndBlockedChatsSaySoInsteadOfTheMessage() {
        var request = last("c:a")
        request.messageRequest = true
        XCTAssertEqual(line(request), .message("Message request", tick: nil))
        var blocked = last("g:00")
        blocked.blocked = true
        XCTAssertEqual(line(blocked), .message("Blocked", tick: nil))
        XCTAssertEqual(line(request, typing: true), .typing)
        var invitation = last("g:00")
        invitation.groupInvitation = true
        invitation.messageRequest = true
        XCTAssertEqual(line(invitation), .message("Group invitation", tick: nil))
        var asked = last("g:00")
        asked.groupJoinRequested = true
        XCTAssertEqual(line(asked), .message("Asked to join", tick: nil))
    }

    func testTheRequestBarNamesThePersonOrTheGroup() {
        var person = conversation("c:a")
        XCTAssertTrue(RequestBar.prompt(person, name: "Alice").hasPrefix("Let Alice message you?"))
        person.blocked = true
        XCTAssertEqual(RequestBar.prompt(person, name: "Alice"), "You blocked Alice.")
        XCTAssertEqual(RequestBar.blockTitle(person, name: "Alice"), "Block Alice?")
        var group = conversation("g:00")
        XCTAssertTrue(RequestBar.prompt(group, name: "G").hasPrefix("Join this group?"))
        XCTAssertTrue(RequestBar.blockMessage(group).contains("You stay a member"))
        group.groupInvitation = true
        XCTAssertTrue(RequestBar.prompt(group, name: "G").hasPrefix("You're invited to join this group."))
        XCTAssertTrue(RequestBar.blockMessage(group).contains("The invitation stays unanswered"), "not a member yet")
        group.groupInvitation = false
        group.groupJoinRequested = true
        XCTAssertEqual(RequestBar.prompt(group, name: "G"), "You asked to join this group. An admin has to let you in.")
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

    func testAnUndecryptableMessageRowSaysOnlyThat() {
        var c = last("g:00", preview: "A message couldn't be decrypted", outgoing: true, status: .read, deleted: true)
        c.lastMessageFile = FileSummary(contentType: "image/png", fileName: nil, isVoiceNote: false)
        c.lastMessageUndecryptable = true
        XCTAssertEqual(line(c), .message("A message couldn't be decrypted", tick: nil))
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

    func testTheMenuBarListsTheChatsWithUnreadMessagesNewestFirst() {
        let now = Date(timeIntervalSince1970: 1_000)
        let list = [conversation("c:1", unread: 2), conversation("c:2"), conversation("c:3", unread: 1)]
        XCTAssertEqual(ChatList.unreadChats(list, now: now, limit: 5).map(\.id), ["c:1", "c:3"])
        XCTAssertEqual(ChatList.unreadChats(list, now: now, limit: 1).map(\.id), ["c:1"])
    }

    func testTheMenuBarSkipsWhatTheBadgeSkipsAndCountsOnlyMessages() {
        let now = Date(timeIntervalSince1970: 1_000)
        var muted = conversation("c:m", unread: 1)
        muted.muteUntil = Int64.max
        var archived = conversation("c:x", unread: 1)
        archived.archived = true
        var marked = conversation("c:u")
        marked.markedUnread = true
        let list = [muted, archived, marked, conversation("c:a", unread: 2)]
        XCTAssertEqual(ChatList.unreadChats(list, now: now, limit: 5).map(\.id), ["c:u", "c:a"])
        XCTAssertEqual(ChatList.menuTitle("Alice", marked), "Alice")
        XCTAssertEqual(ChatList.menuTitle("Alice", conversation("c:a", unread: 2)), "Alice (2)")
    }

    func testDockBadgeIsTheTotalUnread() {
        let now = Date(timeIntervalSince1970: 1_000)
        XCTAssertNil(ChatList.badge([conversation("c:1")], now: now))
        XCTAssertEqual(ChatList.badge([conversation("c:1", unread: 2), conversation("c:2", unread: 5)], now: now), "7")
    }

    func testDockBadgeSkipsMutedAndArchivedChatsAndCountsAMarkedUnreadChatOnce() {
        let now = Date(timeIntervalSince1970: 1_000)
        var muted = conversation("c:m", unread: 4)
        muted.muteUntil = Int64.max
        var archived = conversation("c:x", unread: 2)
        archived.archived = true
        var marked = conversation("c:u")
        marked.markedUnread = true
        XCTAssertEqual(ChatList.badge([conversation("c:a", unread: 2), muted, archived, marked], now: now), "3")
        XCTAssertNil(ChatList.badge([muted, archived], now: now))
        // marked unread with unread messages too: the messages count, not an extra one
        var both = conversation("c:b", unread: 3)
        both.markedUnread = true
        XCTAssertEqual(ChatList.badge([both], now: now), "3")
        // a mute that ran out no longer hides anything
        muted.muteUntil = 999_999
        XCTAssertEqual(ChatList.badge([muted], now: now), "4")
    }

    func testMarkReadOnlyWhenTheNewestMessageIsSeen() {
        func mark(bottom: Bool = true, key: Bool = true, visible: Bool = true, unread: Bool = true) -> Bool {
            ChatList.shouldMarkRead(atBottom: bottom, windowKey: key, windowVisible: visible, unread: unread)
        }
        XCTAssertTrue(mark())
        XCTAssertFalse(mark(bottom: false))
        XCTAssertFalse(mark(key: false))
        // key but not on screen: screen locked or asleep, the user is away
        XCTAssertFalse(mark(visible: false))
        XCTAssertFalse(mark(unread: false))
    }

    func testPinnedChatsLeadTheInboxAndArchivedOnesAreKeptApart() {
        var pinned = conversation("c:p")
        pinned.pinnedOrder = 1
        var archived = conversation("c:x")
        archived.archived = true
        let list = [pinned, conversation("c:a"), archived]  // the core's order: pinned first
        XCTAssertEqual(ChatList.inbox(list).map(\.id), ["c:p", "c:a"])
        XCTAssertEqual(ChatList.archived(list).map(\.id), ["c:x"])
    }

    func testAChatMarkedUnreadCountsAsUnread() {
        var marked = conversation("c:u")
        marked.markedUnread = true
        XCTAssertTrue(ChatList.isUnread(marked))
        XCTAssertFalse(ChatList.isUnread(conversation("c:a")))
        XCTAssertTrue(ChatList.isUnread(conversation("c:a", unread: 1)))
        let list = [conversation("c:a"), marked]
        XCTAssertEqual(ChatList.unreadChats(list, now: Date(), limit: 5).map(\.id), ["c:u"])
        XCTAssertEqual(ChatList.neighbor(in: list, of: "c:a", step: 1, unreadOnly: true), "c:u")
    }

    func testAMuteLastsUntilItsTime() {
        let now = Date(timeIntervalSince1970: 1_000)
        var c = conversation("c:a")
        XCTAssertFalse(ChatList.isMuted(c, now: now))
        c.muteUntil = 1_000_000  // ms: just ran out
        XCTAssertFalse(ChatList.isMuted(c, now: now))
        c.muteUntil = 1_000_001
        XCTAssertTrue(ChatList.isMuted(c, now: now))
        c.muteUntil = Int64.max
        XCTAssertTrue(ChatList.isMuted(c, now: now))
    }

    func testMuteOptionsAreDesktops() {
        let now = Date(timeIntervalSince1970: 1_000)
        XCTAssertEqual(ChatList.MuteOption.allCases.map(\.title), ["1 hour", "8 hours", "1 day", "1 week", "Always"])
        XCTAssertEqual(ChatList.MuteOption.hour.until(now: now), 1_000_000 + 3_600_000)
        XCTAssertEqual(ChatList.MuteOption.eightHours.until(now: now), 1_000_000 + 8 * 3_600_000)
        XCTAssertEqual(ChatList.MuteOption.day.until(now: now), 1_000_000 + 86_400_000)
        XCTAssertEqual(ChatList.MuteOption.week.until(now: now), 1_000_000 + 7 * 86_400_000)
        XCTAssertEqual(ChatList.MuteOption.always.until(now: now), Int64.max)
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
        XCTAssertNil(store.badge)
        store.markRead("c:a")  // nothing unread
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.markedRead, ["c:a"])
    }

    func testAFailedMarkReadIsShownAndCanBeRetried() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a", unread: 2)])]
        fake.markReadError = TestError(description: "offline")
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        store.markRead("c:a")
        await eventually { store.lastError != nil }
        fake.markReadError = nil
        store.markRead("c:a")
        await eventually { fake.markedRead == ["c:a", "c:a"] }
    }

    func testSearchNarrowsKeyboardNavigationAndReturnOpensAMatch() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [
            .success([conversation("c:alice"), conversation("c:bob"), conversation("c:carol")])
        ]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 3 }
        store.searchQuery = "c:b"
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:bob"])
        store.selectChat(at: 1)
        XCTAssertEqual(store.selectedId, "c:bob")
        store.selectNeighbor(step: 1, unreadOnly: false)  // wraps within the matches only
        XCTAssertEqual(store.selectedId, "c:bob")
        // ↩ opens the first match when the open chat isn't one, ends the search, moves to the composer
        store.select("c:alice")
        store.searchQuery = "car"
        store.openSearchResult()
        XCTAssertEqual(store.selectedId, "c:carol")
        XCTAssertEqual(store.searchQuery, "")
        XCTAssertTrue(store.composerWantsFocus)
    }

    func testOnlyExplicitRequestsMoveFocusToTheComposer() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a"), conversation("c:b")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 2 }
        // arrows in the search or the list switch chats but leave focus where it is
        store.selectNeighbor(step: 1, unreadOnly: false)
        XCTAssertFalse(store.composerWantsFocus)
        store.focusComposer()
        XCTAssertTrue(store.composerWantsFocus)
        store.composerTookFocus()
        XCTAssertFalse(store.composerWantsFocus)
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

    func testChatFlagsGoToTheCoreAndAFailureIsShown() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        store.setArchived("c:a", true)
        store.setPinned("c:a", true)
        store.mute("c:a", for: .always)
        store.unmute("c:a")
        store.setMarkedUnread("c:a", true)
        await eventually { fake.flagCalls.count == 5 }
        XCTAssertEqual(
            fake.flagCalls.sorted(),  // each change is its own task: no order between them
            [
                "archived c:a true", "pinned c:a true", "mute c:a \(Int64.max)", "mute c:a 0",
                "markedUnread c:a true",
            ].sorted())
        XCTAssertNil(store.lastError)
        fake.changeError = TestError(description: "up to 4 chats can be pinned")
        store.setPinned("c:a", true)
        await eventually { store.lastError != nil }
        XCTAssertEqual(store.lastError, "Couldn't pin the chat: up to 4 chats can be pinned")
        // the next change that works takes the message away
        fake.changeError = nil
        store.setArchived("c:a", true)
        await eventually { store.lastError == nil }
    }

    func testBlockingAndAnsweringRequestsGoToTheCore() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a"), conversation("c:b")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 2 }
        store.block("c:a")
        store.acceptRequest("c:a")
        await eventually { fake.flagCalls.count == 2 }
        store.select("c:b")
        store.deleteChat("c:b")
        XCTAssertNil(store.selectedId, "a deleted chat closes")
        await eventually { fake.flagCalls.count == 3 }
        XCTAssertEqual(fake.flagCalls, ["block c:a", "accept c:a", "delete c:b"])
    }

    func testTheStorageWritesSwitchReadsAndWritesTheCore() async {
        let fake = FakeClient(linked: true)
        fake.storageWritesAnswer = false
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.storageWrites == false }
        store.setStorageWrites(true)
        XCTAssertTrue(store.storageWrites)
        await eventually { fake.storageWritesSet == [true] }
        // a refusal puts the switch back and says why
        fake.changeError = TestError(description: "disk full")
        store.setStorageWrites(false)
        await eventually { store.lastError != nil }
        XCTAssertTrue(store.storageWrites)
    }

    func testFlagChangesRunInTheOrderTheyWereMade() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        fake.slowFirstFlag = .milliseconds(100)
        store.mute("c:a", for: .always)
        store.unmute("c:a")
        await eventually { fake.flagCalls.count == 2 }
        XCTAssertEqual(fake.flagCalls, ["mute c:a \(Int64.max)", "mute c:a 0"])
    }

    func testAWhitespaceSearchKeepsTheInbox() async {
        let fake = FakeClient(linked: true)
        var archived = conversation("c:x")
        archived.archived = true
        fake.conversationAnswers = [.success([conversation("c:a"), archived])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 2 }
        store.searchQuery = "  "
        XCTAssertFalse(store.isSearching)
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:a"])
        store.searchQuery = " c:x "
        XCTAssertTrue(store.isSearching)
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:x"])
    }

    func testMarkingTheOpenChatUnreadClosesIt() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a"), conversation("c:b")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 2 }
        store.select("c:a")
        store.setMarkedUnread("c:b", true)
        XCTAssertEqual(store.selectedId, "c:a", "another chat: nothing closes")
        store.setMarkedUnread("c:a", true)
        XCTAssertNil(store.selectedId, "the open chat would be read again at once")
        await eventually { fake.flagCalls.count == 2 }
    }

    func testTheBadgeRecountsWhenAMuteRunsOut() async {
        let fake = FakeClient(linked: true)
        var muted = conversation("c:m", unread: 2)
        muted.muteUntil = Int64(Date().timeIntervalSince1970 * 1000) + 150
        fake.conversationAnswers = [.success([muted])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        XCTAssertNil(store.badge)
        await eventually { store.badge == "2" }
    }

    func testTheArchiveClosesWhenItsLastChatComesBack() async {
        let fake = FakeClient(linked: true)
        var archived = conversation("c:x")
        archived.archived = true
        fake.conversationAnswers = [.success([archived]), .success([conversation("c:x")])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        store.showArchive(true)
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:x"])
        store.handle(.conversationsChanged)
        await eventually { !store.showingArchive }
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:x"])
        XCTAssertEqual(store.archivedCount, 0)
    }

    func testTheArchiveIsItsOwnListExceptWhenSearching() async {
        let fake = FakeClient(linked: true)
        var archived = conversation("c:x")
        archived.archived = true
        fake.conversationAnswers = [.success([conversation("c:a"), archived])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 2 }
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:a"])
        XCTAssertEqual(store.archivedCount, 1)
        XCTAssertFalse(store.showingArchive)
        store.showArchive(true)
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:x"])
        store.selectChat(at: 1)
        XCTAssertEqual(store.selectedId, "c:x")
        store.searchQuery = "c:"
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:a", "c:x"], "a search looks everywhere")
        store.searchQuery = ""
        store.showArchive(false)
        XCTAssertEqual(store.visibleConversations.map(\.id), ["c:a"])
    }

    func testMarkReadRunsForAChatMarkedUnread() async {
        let fake = FakeClient(linked: true)
        var marked = conversation("c:a")
        marked.markedUnread = true
        fake.conversationAnswers = [.success([marked])]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        await eventually { store.conversations.count == 1 }
        XCTAssertEqual(store.badge, "1")
        store.markRead("c:a")
        await eventually { fake.markedRead == ["c:a"] }
    }
}
