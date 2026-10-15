import XCTest

@testable import Semaphore

final class NotificationsTests: XCTestCase {
    private func request(
        _ setting: NotificationContent, group: String? = nil, text: String? = "hi", reaction: String? = nil
    ) -> NotificationRequest? {
        Notifications.request(
            setting: setting, conversationId: "c:a", id: "c:a|a|1", sender: "Alice", groupTitle: group, text: text,
            reaction: reaction)
    }

    func testFullContentNamesTheSenderAndGroupLikeDesktop() {
        let r = request(.nameAndMessage)
        XCTAssertEqual(r?.title, "Alice")
        XCTAssertEqual(r?.body, "hi")
        XCTAssertEqual(r?.hasActions, true)
        XCTAssertEqual(r?.conversationId, "c:a")
        XCTAssertEqual(request(.nameAndMessage, group: "Climbing crew")?.title, "Alice in Climbing crew")
        XCTAssertEqual(request(.nameAndMessage, text: nil)?.body, "Attachment")
    }

    func testReactionsSayWhoReactedToWhat() {
        XCTAssertEqual(request(.nameAndMessage, text: "dinner?", reaction: "👍")?.body, "Alice reacted 👍 to: dinner?")
    }

    func testLessContentHidesTheTextThenTheName() {
        let nameOnly = request(.nameOnly, reaction: "👍")
        XCTAssertEqual(nameOnly?.title, "Alice")
        XCTAssertEqual(nameOnly?.body, "New Message")
        XCTAssertEqual(nameOnly?.hasActions, false)
        let nothing = request(.noNameOrMessage, group: "Climbing crew")
        XCTAssertEqual(nothing?.title, "Semaphore")
        XCTAssertEqual(nothing?.body, "New Message")
        XCTAssertEqual(nothing?.hasActions, false)
        XCTAssertNil(request(.off))
    }

    func testTheDefaultIsFullContent() {
        let defaults = UserDefaults(suiteName: "semaphore.tests.\(UUID())")!
        XCTAssertEqual(NotificationContent.current(defaults), .nameAndMessage)
        defaults.set("name", forKey: NotificationContent.key)
        XCTAssertEqual(NotificationContent.current(defaults), .nameOnly)
    }
}

final class FakeNotifier: Notifier, @unchecked Sendable {
    private(set) var authorized = 0
    private(set) var posted: [NotificationRequest] = []
    private(set) var removed: [String] = []
    func authorize() { authorized += 1 }
    func post(_ request: NotificationRequest) { posted.append(request) }
    func remove(conversationId: String) { removed.append(conversationId) }
}

@MainActor
final class AppStoreNotificationTests: XCTestCase {
    private var active = false
    private var defaults: UserDefaults!

    override func setUp() {
        defaults = UserDefaults(suiteName: "semaphore.tests.\(UUID())")!
        NotificationContent.defaults = defaults
    }

    override func tearDown() { NotificationContent.defaults = .standard }

    private func store(_ fake: FakeClient, _ notifier: FakeNotifier) async -> AppStore {
        let store = AppStore(open: { _ in fake }, notifier: notifier, isAppActive: { [unowned self] in self.active })
        await store.boot()
        return store
    }

    private func newMessage(_ conv: String, _ author: String, _ sentAt: Int64) -> CoreEvent {
        .newMessage(conversationId: conv, key: MessageKey(author: author, sentAt: sentAt))
    }

    func testAMessageNotifiesOnlyWhileTheAppIsInTheBackground() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:alice"] = "Alice"
        fake.messageAnswers["c:alice"] = [message("c:alice", "alice", 1, "hi"), message("c:alice", "alice", 2, "yo")]
        let notifier = FakeNotifier()
        let store = await store(fake, notifier)
        XCTAssertEqual(notifier.authorized, 1)
        store.handle(newMessage("c:alice", "alice", 1))
        await eventually { notifier.posted.count == 1 }
        XCTAssertEqual(notifier.posted.first?.title, "Alice")
        XCTAssertEqual(notifier.posted.first?.body, "hi")
        active = true
        store.handle(newMessage("c:alice", "alice", 2))
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(notifier.posted.count, 1)
    }

    func testGroupMessagesNameSenderAndGroupAndOffMeansNone() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:alice"] = "Alice"
        fake.titles["g:00"] = "Climbing crew"
        fake.messageAnswers["g:00"] = [message("g:00", "alice", 1, "who's in?"), message("g:00", "alice", 2)]
        let notifier = FakeNotifier()
        let store = await store(fake, notifier)
        store.handle(newMessage("g:00", "alice", 1))
        await eventually { notifier.posted.count == 1 }
        XCTAssertEqual(notifier.posted.first?.title, "Alice in Climbing crew")
        defaults.set(NotificationContent.off.rawValue, forKey: NotificationContent.key)
        store.handle(newMessage("g:00", "alice", 2))
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(notifier.posted.count, 1)
    }

    func testDeletedMessagesDontNotifyButReactionsToMineDo() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:alice"] = "Alice"
        var gone = message("c:alice", "alice", 1)
        gone.deleted = true
        fake.messageAnswers["c:alice"] = [gone, message("c:alice", "me-aci", 2, "dinner?", outgoing: true)]
        let notifier = FakeNotifier()
        let store = await store(fake, notifier)
        store.handle(newMessage("c:alice", "alice", 1))
        store.handle(
            .newReaction(
                conversationId: "c:alice", target: MessageKey(author: "me-aci", sentAt: 2), reactor: "alice",
                emoji: "👍"))
        await eventually { notifier.posted.count == 1 }
        XCTAssertEqual(notifier.posted.first?.body, "Alice reacted 👍 to: dinner?")
    }

    func testNotificationsGoAwayOnceTheChatIsReadOrOpened() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = [message("c:a", "a", 1)]
        fake.conversationAnswers = [.success([conversation("c:a", unread: 1), conversation("c:b")])]
        let notifier = FakeNotifier()
        let store = await store(fake, notifier)
        await eventually { store.conversations.count == 2 }
        store.handle(newMessage("c:a", "a", 1))
        await eventually { notifier.posted.count == 1 }
        // read on the phone: the list refresh shows it
        fake.conversationAnswers = [.success([conversation("c:a"), conversation("c:b")])]
        store.handle(.conversationsChanged)
        await eventually { notifier.removed == ["c:a"] }
        store.select("c:b")
        XCTAssertEqual(notifier.removed, ["c:a", "c:b"])
    }

    func testReplyFromANotificationSendsAndMarksTheChatRead() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a", unread: 1)])]
        let notifier = FakeNotifier()
        let store = await store(fake, notifier)
        await eventually { store.conversations.count == 1 }
        await store.replyFromNotification(conversationId: "c:a", text: " on my way ")
        XCTAssertEqual(fake.sent.map(\.body), ["on my way"])
        XCTAssertEqual(fake.sent.first?.conversationId, "c:a")
        await eventually { fake.markedRead == ["c:a"] }
        await store.markReadFromNotification(conversationId: "c:a")
        XCTAssertTrue(notifier.removed.contains("c:a"))
    }

    func testOpeningANotificationOpensItsChat() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake, FakeNotifier())
        await store.openFromNotification(conversationId: "c:a")
        XCTAssertEqual(store.selectedId, "c:a")
        XCTAssertTrue(store.composerWantsFocus)
    }
}
