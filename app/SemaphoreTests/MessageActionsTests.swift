import XCTest

@testable import Semaphore

final class MessageActionsTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_000_000)
    private var nowMs: Int64 { Int64(now.timeIntervalSince1970 * 1000) }

    private func mine(_ agoMs: Int64, status: MessageStatus = .sent, body: String? = "hi", deleted: Bool = false)
        -> Message
    {
        var m = message("c:a", "me", nowMs - agoMs, body ?? "", outgoing: true)
        m.body = body
        m.status = status
        m.deleted = deleted
        return m
    }

    func testOnlyOwnSentRecentMessagesCanBeChanged() {
        let hour: Int64 = 3_600_000
        XCTAssertTrue(MessageActions.canDelete(mine(hour), me: "me", now: now))
        XCTAssertTrue(MessageActions.canEdit(mine(hour), me: "me", now: now))
        XCTAssertFalse(MessageActions.canDelete(mine(25 * hour), me: "me", now: now), "older than 24 h")
        XCTAssertFalse(MessageActions.canDelete(mine(hour, status: .pending), me: "me", now: now))
        XCTAssertFalse(MessageActions.canDelete(mine(hour, status: .failed), me: "me", now: now))
        XCTAssertFalse(MessageActions.canDelete(mine(hour, deleted: true), me: "me", now: now))
        XCTAssertFalse(MessageActions.canDelete(message("c:a", "alice", nowMs - hour), me: "me", now: now))
        XCTAssertFalse(MessageActions.canDelete(mine(hour), me: nil, now: now))
        // a file without text can be deleted, not edited
        XCTAssertTrue(MessageActions.canDelete(mine(hour, body: nil), me: "me", now: now))
        XCTAssertFalse(MessageActions.canEdit(mine(hour, body: nil), me: "me", now: now))
    }

    func testAFormattedMessageCantBeEditedHereYet() {
        // the composer can't write styles or mentions yet: an edit would strip them
        var m = mine(3_600_000)
        m.ranges = [BodyRange(start: 0, length: 1, style: .bold)]
        XCTAssertTrue(MessageActions.canDelete(m, me: "me", now: now))
        XCTAssertFalse(MessageActions.canEdit(m, me: "me", now: now))
    }

    func testOnlyRealEmojiFromThePaletteBecomeReactions() {
        XCTAssertTrue(MessageActions.isReaction("😂"))
        XCTAssertTrue(MessageActions.isReaction("👍🏽"))
        XCTAssertTrue(MessageActions.isReaction("🇦🇹"))
        XCTAssertTrue(MessageActions.isReaction("❤️"))
        XCTAssertFalse(MessageActions.isReaction("1"))
        XCTAssertFalse(MessageActions.isReaction("#"))
        XCTAssertFalse(MessageActions.isReaction("©"))
        XCTAssertFalse(MessageActions.isReaction("a"))
    }

    func testPickingYourReactionAgainTakesItBackAndAnotherReplacesIt() {
        let reactions = [Reaction(reactor: "me", emoji: "❤️"), Reaction(reactor: "alice", emoji: "👍")]
        XCTAssertEqual(MessageActions.mine(reactions, me: "me"), "❤️")
        XCTAssertTrue(MessageActions.reaction(choosing: "❤️", mine: "❤️") == ("❤️", true))
        XCTAssertTrue(MessageActions.reaction(choosing: "👍", mine: "❤️") == ("👍", false))
        XCTAssertTrue(MessageActions.reaction(choosing: "👍", mine: nil) == ("👍", false))
    }

    func testThePillCountsEachEmojiAndMarksYours() {
        let reactions = [
            Reaction(reactor: "a", emoji: "👍"), Reaction(reactor: "me", emoji: "👍"), Reaction(reactor: "b", emoji: "😂"),
        ]
        let pill = MessageActions.pill(reactions, me: "me")
        XCTAssertEqual(pill.map(\.emoji), ["👍", "😂"])
        XCTAssertEqual(pill.map(\.count), [2, 1])
        XCTAssertEqual(pill.map(\.mine), [true, false])
    }

    func testUpEditsYourLastEditableMessage() {
        let hour: Int64 = 3_600_000
        let older = mine(3 * hour)
        var failed = mine(hour, status: .failed)
        failed.sentAt += 1
        let theirs = message("c:a", "alice", nowMs - 1)
        XCTAssertEqual(
            MessageActions.lastEditable([older, failed, theirs], me: "me", now: now)?.sentAt, older.sentAt)
        XCTAssertNil(MessageActions.lastEditable([theirs], me: "me", now: now))
    }
}

@MainActor
final class AppStoreMessageActionsTests: XCTestCase {
    private func store(_ fake: FakeClient) async -> AppStore {
        fake.aci = "me"
        fake.messageAnswers["c:a"] = [message("c:a", "alice", 1), message("c:a", "me", 2, "typo", outgoing: true)]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.select("c:a")
        await eventually { store.messages.count == 2 }
        return store
    }

    func testReactingTwiceWithTheSameEmojiTakesItBack() async {
        let fake = FakeClient(linked: true)
        let target = MessageKey(author: "alice", sentAt: 1)
        fake.reactionAnswers[target] = [Reaction(reactor: "me", emoji: "❤️")]
        let store = await store(fake)
        await eventually { store.reactions[target] != nil }
        await store.react(to: store.messages[0], with: "❤️")
        await store.react(to: store.messages[0], with: "😂")
        XCTAssertEqual(fake.reacted.map(\.emoji), ["❤️", "😂"])
        XCTAssertEqual(fake.reacted.map(\.remove), [true, false])
    }

    func testEditingSendsTheNewTextInsteadOfANewMessage() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        let own = store.messages[1]
        store.startEditing(own)
        XCTAssertEqual(store.editing?.sentAt, 2)
        let sent = await store.send(" fixed ")
        XCTAssertTrue(sent)
        XCTAssertEqual(fake.edited.map(\.body), ["fixed"])
        XCTAssertEqual(fake.edited.first?.key, MessageKey(author: "me", sentAt: 2))
        XCTAssertTrue(fake.sent.isEmpty)
        XCTAssertNil(store.editing)
    }

    func testAFailedChangeSaysWhyAndKeepsEditing() async {
        let fake = FakeClient(linked: true)
        fake.changeError = CoreError.InvalidInput(message: "messages can only be changed for 24 hours")
        let store = await store(fake)
        store.startEditing(store.messages[1])
        let sent = await store.send("fixed")
        XCTAssertFalse(sent)
        XCTAssertNotNil(store.editing)
        XCTAssertEqual(store.lastError, "Couldn't edit the message: messages can only be changed for 24 hours")
        await store.deleteForEveryone(store.messages[1])
        XCTAssertTrue(fake.deletedForEveryone.isEmpty)
    }

    func testDeleteForEveryone() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        await store.deleteForEveryone(store.messages[1])
        XCTAssertEqual(fake.deletedForEveryone, [MessageKey(author: "me", sentAt: 2)])
    }

    func testReactingAgainBeforeTheFirstOneIsSentTakesItBack() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        await store.react(to: store.messages[0], with: "❤️")  // queued: not in the stored reactions yet
        XCTAssertEqual(store.myReaction(to: store.messages[0]), "❤️")
        await store.react(to: store.messages[0], with: "❤️")
        XCTAssertEqual(fake.reacted.map(\.remove), [false, true])
        XCTAssertNil(store.myReaction(to: store.messages[0]))
    }

    func testReplyingLeavesEditModeAndFilesWaitTillTheEditIsDone() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.startEditing(store.messages[1])
        store.addFiles([
            OutgoingFile(path: "/tmp/a.png", contentType: "image/png", width: 1, height: 1, isVoiceNote: false)
        ])
        XCTAssertTrue(store.outgoingFiles.isEmpty, "an edit can't add files")
        store.reply(to: store.messages[0])
        XCTAssertNil(store.editing)
        XCTAssertEqual(store.replyTo?.sentAt, 1)
    }

    func testAMessageDeletedWhileBeingEditedEndsTheEdit() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.startEditing(store.messages[1])
        var gone = message("c:a", "me", 2, "typo", outgoing: true)
        gone.deleted = true
        gone.body = nil
        fake.messageAnswers["c:a"] = [message("c:a", "alice", 1), gone]
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "me", sentAt: 2)))
        await eventually { store.editing == nil }
    }
}
