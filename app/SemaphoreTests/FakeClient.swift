import Foundation
import XCTest

@testable import Semaphore

/// Records what the bridge asks of the core and returns scripted answers.
final class FakeClient: ClientProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var _linked: Bool
    private(set) var linkCalls: [String] = []
    private(set) var startCalls = 0
    private(set) var conversationCalls = 0
    var linkError: Error?
    var startError: Error?
    /// Each `conversations()` call takes the next answer; the last one repeats.
    var conversationAnswers: [Result<[Conversation], Error>] = [.success([])]
    /// When set, `conversations()` call number `n` (1-based) waits for `release` first.
    var holdCall: Int?
    private var held: CheckedContinuation<Void, Never>?

    init(linked: Bool) { _linked = linked }

    var isHolding: Bool { lock.withLock { held != nil } }

    func release() {
        lock.lock()
        let c = held
        held = nil
        lock.unlock()
        c?.resume()
    }

    func isLinked() -> Bool { lock.withLock { _linked } }
    func setLinked(_ v: Bool) { lock.withLock { _linked = v } }

    func link(deviceName: String) throws {
        lock.withLock { linkCalls.append(deviceName) }
        if let linkError { throw linkError }
    }

    func start() throws {
        lock.withLock { startCalls += 1 }
        if let startError { throw startError }
    }

    func conversations() async throws -> [Conversation] {
        let n = lock.withLock { () -> Int in
            conversationCalls += 1
            return conversationCalls
        }
        if n == holdCall {
            await withCheckedContinuation { c in lock.withLock { held = c } }
        }
        let answer = lock.withLock { conversationAnswers[min(n, conversationAnswers.count) - 1] }
        return try answer.get()
    }

    var aci: String? = "me-aci"
    private(set) var sent: [(conversationId: String, body: String, quote: Quote?)] = []
    private(set) var retried: [MessageKey] = []
    var titles: [String: String] = [:]
    var messageAnswers: [String: [Message]] = [:]

    func ownAci() -> String? { aci }

    var sendError: Error?
    func sendText(conversationId: String, body: String, quote: Quote?) async throws -> MessageKey {
        if let sendError { throw sendError }
        lock.withLock { sent.append((conversationId, body, quote)) }
        return MessageKey(author: aci ?? "", sentAt: Int64(sent.count))
    }

    func retry(key: MessageKey) async throws -> Bool {
        lock.withLock { retried.append(key) }
        return true
    }

    func react(target: MessageKey, emoji: String, remove: Bool) async throws {}
    func edit(key: MessageKey, newBody: String) async throws {}
    func deleteForEveryone(key: MessageKey) async throws {}
    private(set) var markedRead: [String] = []
    var markReadError: Error?
    func markRead(conversationId: String) async throws -> UInt32 {
        lock.withLock { markedRead.append(conversationId) }
        if let markReadError { throw markReadError }
        return 0
    }
    func setTyping(conversationId: String, started: Bool) throws {}
    func downloadAttachment(id: Int64) throws {}
    func sendAttachments(conversationId: String, files: [OutgoingFile], caption: String?, quote: Quote?) async throws
        -> MessageKey
    { MessageKey(author: aci ?? "", sentAt: 0) }
    func attachments(key: MessageKey) async throws -> [Attachment] { [] }
    func attachment(id: Int64) async throws -> Attachment? { nil }
    func linkPreview(key: MessageKey) async throws -> LinkPreview? { nil }

    private(set) var titleCalls = 0
    var reactionAnswers: [MessageKey: [Reaction]] = [:]
    private(set) var reactionCalls = 0

    func title(conversationId: String) async throws -> String? {
        lock.withLock { titleCalls += 1 }
        return titles[conversationId]
    }

    func message(key: MessageKey) async throws -> Message? {
        lock.withLock {
            messageAnswers.values.flatMap { $0 }.first { $0.author == key.author && $0.sentAt == key.sentAt }
        }
    }

    private(set) var messageCalls: [(conversationId: String, before: MessageKey?)] = []
    /// When set, `messages()` for this conversation waits for `releaseMessages()` first.
    var holdMessagesFor: String?
    private var heldMessages: CheckedContinuation<Void, Never>?
    var isHoldingMessages: Bool { lock.withLock { heldMessages != nil } }

    func releaseMessages() {
        let c = lock.withLock { () -> CheckedContinuation<Void, Never>? in
            defer { heldMessages = nil }
            return heldMessages
        }
        c?.resume()
    }

    func messages(conversationId: String, before: MessageKey?, limit: UInt32) async throws -> [Message] {
        lock.withLock { messageCalls.append((conversationId, before)) }
        if conversationId == holdMessagesFor {
            await withCheckedContinuation { c in lock.withLock { heldMessages = c } }
        }
        let all = lock.withLock { messageAnswers[conversationId] ?? [] }
        let older = before.map { b in all.filter { ($0.sentAt, $0.author) < (b.sentAt, b.author) } } ?? all
        return Array(older.suffix(Int(limit)))
    }
    func reactions(key: MessageKey) async throws -> [Reaction] {
        lock.withLock {
            reactionCalls += 1
            return reactionAnswers[key] ?? []
        }
    }
    func search(query: String, limit: UInt32) async throws -> [Message] { [] }
}

func conversation(_ id: String, unread: UInt32 = 0) -> Conversation {
    Conversation(
        id: id, kind: id.hasPrefix("g:") ? .group : .direct, title: id, lastMessageAt: 1, lastMessagePreview: "",
        unreadCount: unread, lastMessageAuthor: "a", lastMessageOutgoing: false, lastMessageStatus: .delivered,
        lastMessageDeleted: false)
}

final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func increment() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

struct TestError: Error, CustomStringConvertible {
    let description: String
}

/// Polls the main actor until `condition` holds (or fails the test after ~2 s).
@MainActor
func eventually(
    _ condition: @MainActor () -> Bool, file: StaticString = #filePath, line: UInt = #line
) async {
    for _ in 0..<200 {
        if condition() { return }
        try? await Task.sleep(for: .milliseconds(10))
    }
    XCTFail("condition not met in time", file: file, line: line)
}

func message(_ conv: String, _ author: String, _ sentAt: Int64, _ body: String = "x", outgoing: Bool = false) -> Message
{
    Message(
        conversationId: conv, author: author, sentAt: sentAt, receivedAt: sentAt, body: body, isOutgoing: outgoing,
        status: outgoing ? .sent : .delivered, edited: false, deleted: false, quote: nil)
}
