import XCTest

@testable import Hush

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

    func testStyledMessagesAndOnesWithMentionsCanBeEdited() {
        var m = mine(3_600_000)
        m.ranges = [
            BodyRange(start: 0, length: 1, style: .bold), BodyRange(start: 0, length: 1, style: .mention(aci: "bob")),
        ]
        XCTAssertTrue(MessageActions.canEdit(m, me: "me", now: now))
        // a mention the composer can't hold would be lost by the edit
        m.ranges = [BodyRange(start: 0, length: 2, style: .mention(aci: "bob"))]
        XCTAssertFalse(MessageActions.canEdit(m, me: "me", now: now))
        m.ranges = [BodyRange(start: 50, length: 1, style: .mention(aci: "bob"))]
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

    func testARepliedStoryCantBeEdited() {
        // an edit would go without the story it answers
        var reply = mine(3_600_000)
        reply.story = MessageKey(author: "alice", sentAt: 1)
        XCTAssertTrue(MessageActions.canDelete(reply, me: "me", now: now))
        XCTAssertFalse(MessageActions.canEdit(reply, me: "me", now: now))
        let older = mine(2 * 3_600_000)
        XCTAssertEqual(MessageActions.lastEditable([older, reply], me: "me", now: now)?.sentAt, older.sentAt)
    }
}

@MainActor
final class AppStoreMessageActionsTests: XCTestCase {
    private func store(_ fake: FakeClient, messages: [Message]? = nil) async -> AppStore {
        fake.aci = "me"
        let messages = messages ?? [message("c:a", "alice", 1), message("c:a", "me", 2, "typo", outgoing: true)]
        fake.messageAnswers["c:a"] = messages
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.select("c:a")
        await eventually { store.messages.count == min(messages.count, 100) }
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

    func testAnEditKeepsItsStyles() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        var own = store.messages[1]
        own.body = "bold move"
        own.ranges = [BodyRange(start: 0, length: 4, style: .bold)]
        store.startEditing(own)
        // what the composer holds, saved unchanged
        let message = ComposerText.message(from: ComposerText.attributed(own.body!, ranges: own.ranges))
        await store.send(message.body, ranges: message.ranges)
        XCTAssertEqual(fake.edited.map(\.body), ["bold move"])
        XCTAssertEqual(fake.editedRanges, [own.ranges])
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
        XCTAssertEqual(
            store.lastError, "The message you were editing was deleted or disappeared, so the edit wasn't sent.")
    }

    /// What Remove in the who-reacted list sends (the button itself, `unreact`, isn't tested here).
    func testSendingARemovalNeverAddsEvenAfterATakeBack() async {
        let fake = FakeClient(linked: true)
        let target = MessageKey(author: "alice", sentAt: 1)
        fake.reactionAnswers[target] = [Reaction(reactor: "me", emoji: "❤️")]
        let store = await store(fake)
        await eventually { store.reactions[target] != nil }
        await store.react(to: store.messages[0], with: "❤️")  // taken back in the picker, not reloaded yet
        // the who-reacted list still shows "You ❤️": a removal from there must not send it again
        await store.sendReaction(to: store.messages[0], emoji: "❤️", remove: true)
        XCTAssertEqual(fake.reacted.map(\.remove), [true, true])
        XCTAssertNil(store.myReaction(to: store.messages[0]))
    }

    func testAReplyToAMessageDeletedForEveryoneEnds() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.reply(to: store.messages[0])
        var gone = message("c:a", "alice", 1)
        gone.deleted = true
        gone.body = nil
        fake.messageAnswers["c:a"] = [gone, message("c:a", "me", 2, "typo", outgoing: true)]
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "alice", sentAt: 1)))
        await eventually { store.replyTo == nil }
        XCTAssertNil(store.lastError, "a reply ends without a word")
    }

    func testAReplyOrEditStaysWhenAnotherMessageWasDeleted() async {
        let fake = FakeClient(linked: true)
        var tombstone = message("c:a", "alice", 0)
        tombstone.deleted = true
        tombstone.body = nil
        let store = await store(
            fake, messages: [tombstone, message("c:a", "alice", 1), message("c:a", "me", 2, "typo", outgoing: true)])
        store.reply(to: store.messages.first { $0.sentAt == 1 }!)
        fake.messageAnswers["c:a"]?.append(message("c:a", "alice", 3))
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "alice", sentAt: 3)))
        await eventually { store.messages.count == 4 }
        XCTAssertEqual(store.replyTo?.sentAt, 1)

        store.startEditing(store.messages.first { $0.sentAt == 2 }!)
        fake.messageAnswers["c:a"]?.append(message("c:a", "alice", 4))
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "alice", sentAt: 4)))
        await eventually { store.messages.count == 5 }
        XCTAssertEqual(store.editing?.sentAt, 2)
        XCTAssertNil(store.lastError)
    }

    func testAReplyToAnOlderMessageDeletedForEveryoneEnds() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake, messages: (1...150).map { message("c:a", "a", Int64($0)) })
        await store.loadOlder()
        store.reply(to: store.messages.first { $0.sentAt == 5 }!)
        fake.messageAnswers["c:a"]?[4].deleted = true
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 5)))
        await eventually { store.replyTo == nil }
    }

    func testAReplyToAMessageThatLeftTheLoadedWindowEndsWhenItIsDeleted() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake, messages: (1...150).map { message("c:a", "a", Int64($0)) })
        await store.loadOlder()
        store.reply(to: store.messages.first { $0.sentAt == 5 }!)
        // a burst of new messages restarts the window at the newest page, without the one replied to
        fake.messageAnswers["c:a"] = (1...300).map { message("c:a", "a", Int64($0)) }
        fake.messageAnswers["c:a"]?[4].deleted = true
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "a", sentAt: 5)))
        await eventually { store.replyTo == nil }
        XCTAssertFalse(store.messages.contains { $0.sentAt == 5 })
    }

    func testDeletingTheMessageBeingEditedEndsTheEditQuietly() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.startEditing(store.messages[1])
        await store.deleteForEveryone(store.messages[1])
        XCTAssertNil(store.editing)
        var gone = message("c:a", "me", 2, "typo", outgoing: true)
        gone.deleted = true
        fake.messageAnswers["c:a"] = [message("c:a", "alice", 1), gone]
        store.handle(.messageChanged(conversationId: "c:a", key: MessageKey(author: "me", sentAt: 2)))
        await eventually { store.messages.last?.deleted == true }
        XCTAssertNil(store.lastError)
    }

    func testAnExpiredMessageEndsTheReplyToItAndTheEditOfIt() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        let (theirs, ours) = (store.messages[0], store.messages[1])
        store.reply(to: theirs)
        store.handle(.messageExpired(conversationId: "c:a", key: MessageKey(author: "bob", sentAt: 1)))
        XCTAssertNotNil(store.replyTo, "another message expired")
        store.handle(.messageExpired(conversationId: "c:a", key: MessageKey(author: "alice", sentAt: 1)))
        XCTAssertNil(store.replyTo)
        XCTAssertNil(store.lastError, "a reply ends without a word")

        store.startEditing(ours)
        store.handle(.messageExpired(conversationId: "c:a", key: MessageKey(author: "me", sentAt: 2)))
        XCTAssertNil(store.editing)
        XCTAssertEqual(
            store.lastError, "The message you were editing was deleted or disappeared, so the edit wasn't sent.")
    }
}
