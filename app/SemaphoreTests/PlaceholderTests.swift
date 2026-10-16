import XCTest

@testable import Semaphore

final class PlaceholderTests: XCTestCase {
    func testThingsWeCantShowYetAreWordedLikeDesktop() {
        XCTAssertEqual(Placeholder.text(.sticker(emoji: "🙂")), "🙂 Sticker message")
        XCTAssertEqual(Placeholder.text(.sticker(emoji: nil)), "Sticker message")
        XCTAssertEqual(Placeholder.text(.contact(name: "Ada Lovelace")), "👤 Ada Lovelace")
        XCTAssertEqual(Placeholder.text(.contact(name: nil)), "👤 Contact")
        XCTAssertEqual(Placeholder.text(.payment), "💳 Payment")
        XCTAssertEqual(Placeholder.text(.poll(question: "Pizza?")), "📊 Poll: Pizza?")
        XCTAssertEqual(Placeholder.text(.giftBadge), "✨ Gift badge")
        XCTAssertNil(Placeholder.text(.normal))
    }

    func testViewOnceMediaOffersOneLookThenSaysViewed() {
        let file = { (state: AttachmentState, type: String) in
            Attachment(
                id: 7, message: MessageKey(author: "a", sentAt: 1), contentType: type, fileName: nil, size: 1,
                width: nil, height: nil, isVoiceNote: false, path: state == .ready ? "/x" : nil, state: state)
        }
        var m = message("c:a", "a", 1)
        m.kind = .viewOnce(viewed: false)
        XCTAssertEqual(ViewOnce.state(m, files: [file(.ready, "image/jpeg")]), .open(file(.ready, "image/jpeg")))
        XCTAssertEqual(ViewOnce.label(.open(file(.ready, "image/jpeg"))), "View photo")
        XCTAssertEqual(ViewOnce.label(.open(file(.ready, "video/mp4"))), "View video")
        XCTAssertEqual(ViewOnce.state(m, files: [file(.pending, "image/jpeg")]), .download(7))
        XCTAssertEqual(ViewOnce.state(m, files: [file(.failed, "image/jpeg")]), .download(7))
        XCTAssertEqual(ViewOnce.state(m, files: [file(.downloading, "image/jpeg")]), .downloading)
        XCTAssertEqual(ViewOnce.state(m, files: []), .viewed, "nothing left to open")
        m.kind = .viewOnce(viewed: true)
        XCTAssertEqual(ViewOnce.state(m, files: [file(.ready, "image/jpeg")]), .viewed)
        XCTAssertEqual(ViewOnce.label(.viewed), "Viewed")
        m.isOutgoing = true
        XCTAssertEqual(ViewOnce.state(m, files: []), .sent)
        XCTAssertEqual(Placeholder.text(.viewOnce(viewed: false)), "View-once media")
    }

    @MainActor func testOpeningViewOnceMediaTellsTheCore() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        var m = message("c:a", "a", 9)
        m.kind = .viewOnce(viewed: false)
        await store.viewOnceOpened(m)
        XCTAssertEqual(fake.viewedOnce, [MessageKey(author: "a", sentAt: 9)])
    }

    func testTheChatListAndNotificationsUseTheirWords() {
        var c = conversation("c:a")
        c.lastMessageKind = .poll(question: "Pizza?")
        XCTAssertEqual(
            ChatList.line(c, selected: false, typing: false, draft: "", authorName: "A"),
            .message("📊 Poll: Pizza?", tick: nil))
    }

    func testTheTimerChoicesAreDesktops() {
        XCTAssertEqual(
            Placeholder.timerChoices.map(Placeholder.timerChoice),
            ["Off", "30 seconds", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks"])
        // a custom timer from the phone is offered too, in its place
        XCTAssertEqual(Placeholder.timerChoices(current: 172_800).filter { $0 > 86_400 && $0 < 604_800 }, [172_800])
        XCTAssertEqual(Placeholder.timerChoices(current: 3_600), Placeholder.timerChoices)
    }

    @MainActor func testSettingTheTimerAsksTheCore() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.setTimer("c:a", seconds: 3600)
        await eventually { fake.flagCalls == ["timer c:a 3600"] }
    }

    func testTimerLengthsReadLikeSignals() {
        XCTAssertEqual(Placeholder.duration(30), "30 seconds")
        XCTAssertEqual(Placeholder.duration(60), "1 minute")
        XCTAssertEqual(Placeholder.duration(300), "5 minutes")
        XCTAssertEqual(Placeholder.duration(3600), "1 hour")
        XCTAssertEqual(Placeholder.duration(8 * 3600), "8 hours")
        XCTAssertEqual(Placeholder.duration(86400), "1 day")
        XCTAssertEqual(Placeholder.duration(604_800), "1 week")
        XCTAssertEqual(Placeholder.duration(4 * 604_800), "4 weeks")
        XCTAssertEqual(Placeholder.duration(90), "90 seconds")
    }

    func testTimerChangesSaySoLikeDesktop() {
        func event(_ seconds: Int64) -> ChatEvent {
            ChatEvent(id: 1, conversationId: "c:a", at: 1, kind: .timerChanged, author: "a", value: seconds)
        }
        XCTAssertEqual(
            Placeholder.timerChange(event(604_800), who: "Alice"),
            "Alice set the disappearing message time to 1 week.")
        XCTAssertEqual(
            Placeholder.timerChange(event(3600), who: nil), "You set the disappearing message time to 1 hour.")
        XCTAssertEqual(Placeholder.timerChange(event(0), who: "Alice"), "Alice disabled disappearing messages.")
        XCTAssertEqual(Placeholder.timerChange(event(0), who: nil), "You disabled disappearing messages.")
    }
}

@MainActor
final class AppStoreExpiryTests: XCTestCase {
    func testAnExpiredMessageLeavesTheOpenChatEvenOutsideTheNewestPage() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...150).map { message("c:a", "a", Int64($0)) }
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.select("c:a")
        await eventually { store.messages.count == 100 }
        await store.loadOlder()
        XCTAssertTrue(store.messages.contains { $0.sentAt == 10 })
        fake.messageAnswers["c:a"] = (1...150).filter { $0 != 10 }.map { message("c:a", "a", Int64($0)) }
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 10)))
        await eventually { !store.messages.contains { $0.sentAt == 10 } }
    }

    func testAnExpiredMessageLeavesTheChatSearchAndNotificationCenter() async {
        let fake = FakeClient(linked: true)
        let notifier = FakeNotifier()
        fake.messageAnswers["c:a"] = [message("c:a", "a", 1), message("c:a", "a", 2)]
        let store = AppStore(open: { _ in fake }, notifier: notifier)
        await store.boot()
        store.select("c:a")
        await eventually { store.messages.count == 2 }
        let key = MessageKey(author: "a", sentAt: 1)
        store.handle(.messageExpired(conversationId: "c:a", key: key))
        XCTAssertEqual(store.messages.map(\.sentAt), [2])
        // one prefix covers the message's notification and those of reactions to it, even ones
        // shown before the app started
        // (boot also clears stale call rings, "call|"; that is not this message's doing)
        XCTAssertEqual(notifier.removedPrefixes.filter { $0 != "call|" }, ["c:a|a|1|"])
        XCTAssertTrue(Notifications.id("c:a", key, reactor: nil, emoji: nil).hasPrefix("c:a|a|1|"))
        XCTAssertTrue(Notifications.id("c:a", key, reactor: "b", emoji: "👍").hasPrefix("c:a|a|1|"))
        XCTAssertNotEqual(
            Notifications.id("c:a", key, reactor: "b", emoji: "👍"),
            Notifications.id("c:a", key, reactor: nil, emoji: nil))
    }
}
