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

    var members: [String: [String]] = [:]
    func groupMembers(conversationId: String) async throws -> [String] {
        lock.withLock { members[conversationId] ?? [] }
    }

    var sendError: Error?
    /// Each sent text's or caption's ranges, in order.
    private(set) var sentRanges: [[BodyRange]] = []
    /// Each sent text's link preview, in order.
    private(set) var sentPreviews: [PreviewDraft?] = []
    var previewDrafts: [String: PreviewDraft] = [:]
    private(set) var previewFetches = 0
    func linkPreviewFor(text: String) async throws -> PreviewDraft? {
        lock.withLock {
            previewFetches += 1
            return previewDrafts.first { text.contains($0.key) }?.value
        }
    }
    func sendText(conversationId: String, body: String, ranges: [BodyRange], quote: Quote?, preview: PreviewDraft?)
        async throws -> MessageKey
    {
        if let sendError { throw sendError }
        lock.withLock {
            sent.append((conversationId, body, quote))
            sentRanges.append(ranges)
            sentPreviews.append(preview)
        }
        return MessageKey(author: aci ?? "", sentAt: Int64(sent.count))
    }

    func retry(key: MessageKey) async throws -> Bool {
        lock.withLock { retried.append(key) }
        return true
    }

    private(set) var reacted: [(target: MessageKey, emoji: String, remove: Bool)] = []
    private(set) var edited: [(key: MessageKey, body: String)] = []
    private(set) var deletedForEveryone: [MessageKey] = []
    var changeError: Error?
    func react(target: MessageKey, emoji: String, remove: Bool) async throws {
        if let changeError { throw changeError }
        lock.withLock { reacted.append((target, emoji, remove)) }
    }
    private(set) var editedRanges: [[BodyRange]] = []
    func edit(key: MessageKey, newBody: String, ranges: [BodyRange]) async throws {
        if let changeError { throw changeError }
        lock.withLock {
            edited.append((key, newBody))
            editedRanges.append(ranges)
        }
    }
    private(set) var viewedOnce: [MessageKey] = []
    func viewOnceOpened(key: MessageKey) async throws {
        if let changeError { throw changeError }
        lock.withLock { viewedOnce.append(key) }
    }
    func deleteForEveryone(key: MessageKey) async throws {
        if let changeError { throw changeError }
        lock.withLock { deletedForEveryone.append(key) }
    }
    private(set) var markedRead: [String] = []
    var markReadError: Error?
    func markRead(conversationId: String) async throws -> UInt32 {
        lock.withLock { markedRead.append(conversationId) }
        if let markReadError { throw markReadError }
        return 0
    }
    func setTyping(conversationId: String, started: Bool) throws {}
    /// Chat flag changes, in order ("archived c:a true", "mute c:a 0"…); fail with `changeError`.
    private(set) var flagCalls: [String] = []
    /// The next flag call waits this long first (once), to test ordering.
    var slowFirstFlag: Duration?
    private func flag(_ call: String) async throws {
        if let delay = lock.withLock({ slowFirstFlag.take() }) { try? await Task.sleep(for: delay) }
        if let changeError { throw changeError }
        lock.withLock { flagCalls.append(call) }
    }
    func setWallpaper(conversationId: String, wallpaper: ChatColor?) async throws {
        try await flag("wallpaper \(conversationId) \(wallpaper?.colors ?? [])")
    }
    func setChatColor(conversationId: String, color: ChatColor?) async throws {
        try await flag("color \(conversationId) \(color?.colors ?? [])")
    }
    func setDisappearingTimer(conversationId: String, seconds: UInt32) async throws {
        try await flag("timer \(conversationId) \(seconds)")
    }
    var storyAnswers: [Story] = []
    private(set) var viewedStories: [MessageKey] = []
    func stories() async throws -> [Story] { lock.withLock { storyAnswers } }
    func storyViewed(key: MessageKey) async throws { lock.withLock { viewedStories.append(key) } }
    var usernames: [String: String] = [:]
    func findUsername(username: String) async throws -> String? {
        if username == "bad" { throw CoreError.InvalidInput(message: "a username looks like name.42") }
        return lock.withLock { usernames[username] }
    }
    var stickerPackAnswers: [StickerPackInfo] = []
    private(set) var sentStickers: [(conversationId: String, packId: String, stickerId: UInt32, quote: Quote?)] = []
    private(set) var addedStickerLinks: [String] = []
    func stickerPacks() async throws -> [StickerPackInfo] { lock.withLock { stickerPackAnswers } }
    func addStickerPack(link: String) async throws {
        if link == "bad" { throw CoreError.InvalidInput(message: "not a sticker pack link") }
        lock.withLock { addedStickerLinks.append(link) }
    }
    func sendSticker(conversationId: String, packId: String, stickerId: UInt32, quote: Quote?) async throws
        -> MessageKey
    {
        if let sendError { throw sendError }
        lock.withLock { sentStickers.append((conversationId, packId, stickerId, quote)) }
        return MessageKey(author: aci ?? "", sentAt: Int64(sentStickers.count))
    }
    var polls: [MessageKey: Poll] = [:]
    func poll(key: MessageKey) async throws -> Poll? { lock.withLock { polls[key] } }
    func vote(key: MessageKey, options: [UInt32]) async throws { try await flag("vote \(key.sentAt) \(options)") }
    func endPoll(key: MessageKey) async throws { try await flag("end poll \(key.sentAt)") }
    func setArchived(conversationId: String, archived: Bool) async throws {
        try await flag("archived \(conversationId) \(archived)")
    }
    func setPinned(conversationId: String, pinned: Bool) async throws {
        try await flag("pinned \(conversationId) \(pinned)")
    }
    func setMuteUntil(conversationId: String, untilMs: Int64) async throws {
        try await flag("mute \(conversationId) \(untilMs)")
    }
    func setMarkedUnread(conversationId: String, marked: Bool) async throws {
        try await flag("markedUnread \(conversationId) \(marked)")
    }
    func block(conversationId: String) async throws { try await flag("block \(conversationId)") }
    func acceptRequest(conversationId: String) async throws { try await flag("accept \(conversationId)") }
    func deleteChat(conversationId: String) async throws { try await flag("delete \(conversationId)") }
    var storageWritesAnswer = true
    private(set) var storageWritesSet: [Bool] = []
    func storageWrites() async throws -> Bool { lock.withLock { storageWritesAnswer } }
    func setStorageWrites(on: Bool) async throws {
        if let changeError { throw changeError }
        lock.withLock { storageWritesSet.append(on) }
    }
    /// What the core answers to "is this chat muted now?".
    var muted: Set<String> = []
    func isMuted(conversationId: String) async throws -> Bool { lock.withLock { muted.contains(conversationId) } }
    private(set) var downloads: [Int64] = []
    private(set) var sentFiles: [(conversationId: String, files: [OutgoingFile], caption: String?)] = []
    var attachmentAnswers: [MessageKey: [Attachment]] = [:]
    var previewAnswers: [MessageKey: LinkPreview] = [:]

    var downloadError: Error?
    var sendDelay: Duration?
    func downloadAttachment(id: Int64) throws {
        if let downloadError { throw downloadError }
        lock.withLock { downloads.append(id) }
    }
    func sendAttachments(
        conversationId: String, files: [OutgoingFile], caption: String?, ranges: [BodyRange], quote: Quote?
    ) async throws -> MessageKey {
        if let sendDelay { try? await Task.sleep(for: sendDelay) }
        if let sendError { throw sendError }
        lock.withLock {
            sentFiles.append((conversationId, files, caption))
            sentRanges.append(ranges)
        }
        return MessageKey(author: aci ?? "", sentAt: Int64(sentFiles.count))
    }
    func attachments(key: MessageKey) async throws -> [Attachment] { lock.withLock { attachmentAnswers[key] ?? [] } }
    func attachment(id: Int64) async throws -> Attachment? {
        lock.withLock { attachmentAnswers.values.flatMap { $0 }.first { $0.id == id } }
    }
    func linkPreview(key: MessageKey) async throws -> LinkPreview? { lock.withLock { previewAnswers[key] } }

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
    private(set) var searches: [(query: String, conversationId: String?)] = []
    var searchAnswers: [String: [SearchHit]] = [:]
    var newerCounts: [MessageKey: UInt32] = [:]
    var searchError: Error?
    var countDelays: [MessageKey: Duration] = [:]
    func search(query: String, conversationId: String?, limit: UInt32) async throws -> [SearchHit] {
        if let searchError { throw searchError }
        return lock.withLock {
            searches.append((query, conversationId))
            return (searchAnswers[query] ?? []).filter {
                conversationId == nil || $0.message.conversationId == conversationId
            }
        }
    }
    private(set) var reconnects = 0
    private(set) var sessionResets: [String] = []
    private(set) var unlinks: [Bool] = []
    var unlinkError: Error?
    var eventAnswers: [String: [ChatEvent]] = [:]
    func reconnect() throws { lock.withLock { reconnects += 1 } }
    func resetSession(conversationId: String) throws { lock.withLock { sessionResets.append(conversationId) } }
    func unlink(tellServer: Bool) async throws {
        lock.withLock { unlinks.append(tellServer) }
        if let unlinkError { throw unlinkError }
        setLinked(false)
    }
    func chatEvents(conversationId: String, since: Int64) async throws -> [ChatEvent] {
        lock.withLock { (eventAnswers[conversationId] ?? []).filter { $0.at >= since } }
    }

    private(set) var startCallCalls: [String] = []
    private(set) var acceptCallCalls = 0
    private(set) var hangupCallCalls = 0
    private(set) var setMutedCalls: [Bool] = []
    private(set) var setInputCalls: [String] = []
    private(set) var setOutputCalls: [String] = []
    var callError: Error?
    var audioDevicesAnswer = AudioDevices(inputs: [], outputs: [], currentInput: nil, currentOutput: nil)

    func startCall(conversationId: String) throws {
        if let callError { throw callError }
        lock.withLock { startCallCalls.append(conversationId) }
    }
    func acceptCall() throws {
        if let callError { throw callError }
        lock.withLock { acceptCallCalls += 1 }
    }
    func hangupCall() throws {
        if let callError { throw callError }
        lock.withLock { hangupCallCalls += 1 }
    }
    func setCallMuted(muted: Bool) throws {
        if let callError { throw callError }
        lock.withLock { setMutedCalls.append(muted) }
    }
    func setCallAudioInput(id: String) throws { lock.withLock { setInputCalls.append(id) } }
    func setCallAudioOutput(id: String) throws { lock.withLock { setOutputCalls.append(id) } }
    func callAudioDevices() async throws -> AudioDevices { lock.withLock { audioDevicesAnswer } }

    var historyPendingAnswer = false
    private(set) var cancelHistoryCalls = 0
    private(set) var retryHistoryCalls = 0
    private(set) var relinkCalls = 0
    var relinkError: Error?
    var retryHistoryError: Error?

    func historyPending() async throws -> Bool { lock.withLock { historyPendingAnswer } }
    func cancelHistory() throws { lock.withLock { cancelHistoryCalls += 1 } }
    func retryHistory() throws {
        lock.withLock { retryHistoryCalls += 1 }
        if let retryHistoryError { throw retryHistoryError }
    }
    func relink() async throws {
        lock.withLock { relinkCalls += 1 }
        if let relinkError { throw relinkError }
        setLinked(false)
    }

    func countNewer(conversationId: String, key: MessageKey) async throws -> UInt32 {
        if let delay = lock.withLock({ countDelays[key] }) { try? await Task.sleep(for: delay) }
        return lock.withLock { newerCounts[key] ?? 0 }
    }
}

func conversation(_ id: String, unread: UInt32 = 0) -> Conversation {
    Conversation(
        id: id, kind: id.hasPrefix("g:") ? .group : .direct, title: id, lastMessageAt: 1, lastMessagePreview: "",
        lastMessageRanges: [],
        unreadCount: unread, lastMessageAuthor: "a", lastMessageOutgoing: false, lastMessageStatus: .delivered,
        lastMessageDeleted: false, lastMessageFile: nil, expireTimer: 0, lastMessageKind: .normal,
        lastMessageUndecryptable: false, archived: false, pinnedOrder: nil, muteUntil: 0, markedUnread: false,
        blocked: false,
        whitelisted: false, messageRequest: false, avatar: nil, username: nil, note: nil, chatColor: nil, wallpaper: nil
    )
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
        conversationId: conv, author: author, sentAt: sentAt, receivedAt: sentAt, body: body, ranges: [],
        isOutgoing: outgoing,
        status: outgoing ? .sent : .delivered, edited: false, deleted: false, quote: nil, kind: .normal, expiresIn: 0,
        expiresAt: nil)
}
