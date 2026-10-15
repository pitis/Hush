import Foundation
import OSLog
import Observation

/// The app's state: linking, the conversation list, the open conversation and its messages.
/// Fed by core events; every query result is checked against the current selection.
@MainActor @Observable
final class AppStore {
    enum Screen: Equatable {
        case starting, link, chats
        case failed(String)
    }

    struct LogLine: Identifiable, Equatable {
        let id: Int
        let text: String
    }

    /// Opens the core; injectable so tests can use a fake client.
    typealias Opener = @Sendable (EventListener) throws -> any ClientProtocol

    static let logLimit = 200
    static let pageSize: UInt32 = 100
    static let typingTimeout: TimeInterval = 15
    static let deviceName = "Semaphore"
    /// App state for Console.app (subsystem dev.pitis.semaphore). Never URLs or message content.
    private static let logger = Logger(subsystem: "dev.pitis.semaphore", category: "app")

    private(set) var screen: Screen = .starting
    private(set) var linkURL: String?
    private(set) var connection: ConnectionState = .offline
    private(set) var conversations: [Conversation] = []
    private(set) var log: [LogLine] = []
    private(set) var lastError: String?
    private(set) var selectedId: String?
    /// The open conversation's loaded messages, oldest first.
    private(set) var messages: [Message] = [] {
        didSet { timelineItems = Timeline.items(for: messages) }
    }
    /// Rows for the timeline, recomputed only when `messages` change.
    private(set) var timelineItems: [Timeline.Item] = []
    private(set) var hasOlder = false
    private(set) var reactions: [MessageKey: [Reaction]] = [:]
    var replyTo: Message?
    /// Who is typing where, until when (per sender: in groups several people can type).
    private var typingUntil: [String: [String: Date]] = [:]
    private var senderNames: [String: String] = [:]
    /// Authors whose name lookup found nothing (not asked again).
    private var unnamedAuthors: Set<String> = []
    private var loadingOlder = false
    /// Keys whose message or reactions changed since the last reload of the open chat.
    private var changedKeys: Set<MessageKey> = []
    private var selectionGeneration = 0
    /// The selection generation the in-flight messages load belongs to, if any.
    private var loadingGeneration: Int?
    private var reloadMessagesAgain = false

    private let open: Opener
    private var client: (any ClientProtocol)?
    private var booted = false
    private var nextLogId = 0
    private var refreshing = false
    private var refreshAgain = false

    init(open: @escaping Opener = AppStore.openReal) {
        self.open = open
    }

    /// `~/Library/Application Support/Semaphore`. Changing it orphans the user's data.
    nonisolated static func dataDirectory() throws -> URL {
        try FileManager.default
            .url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("Semaphore", isDirectory: true)
    }

    nonisolated static func openReal(listener: EventListener) throws -> any ClientProtocol {
        try Client.open(dataDir: dataDirectory().path, dbKeyHex: Keychain().dbKey(), listener: listener)
    }

    /// Opens the core (off the main thread), then either starts receiving or starts linking. Runs once.
    func boot() async {
        guard !booted else { return }
        booted = true
        let listener = Listener(bridge: self)
        let open = self.open
        let opened = await Task.detached { Result { try open(listener) } }.value
        do {
            let client = try opened.get()
            self.client = client
            if client.isLinked() {
                screen = .chats
                try client.start()
                refreshConversations()
            } else {
                screen = .link
                try client.link(deviceName: Self.deviceName)
            }
        } catch {
            Self.logger.error("boot failed: \(Self.describe(error), privacy: .private)")
            screen = .failed(Self.describe(error))
        }
    }

    /// Asks the core for a new linking QR code after a failed or expired attempt.
    func retryLink() {
        guard let client, linkURL == nil else { return }
        lastError = nil
        do { try client.link(deviceName: Self.deviceName) } catch { lastError = Self.describe(error) }
    }

    func handle(_ event: CoreEvent, now: Date = .now) {
        switch event {
        case .linkUrl(let url):
            Self.logger.notice("link code ready")
            linkURL = url
            lastError = nil
            screen = .link
        case .linked:
            Self.logger.notice("linked")
            linkURL = nil
            lastError = nil
            screen = .chats
            do { try client?.start() } catch { append("start failed: \(error)") }
        case .connection(let state):
            Self.logger.notice("connection \(String(describing: state), privacy: .public)")
            connection = state
            if state == .unlinked {
                linkURL = nil
                screen = .link
            } else if client?.isLinked() == true, screen != .chats {
                // only a linked core moves us to the chats (a core failure mid-link must not)
                screen = .chats
            }
        case .ready, .conversationsChanged:
            Self.logger.notice("\(event == .ready ? "caught up" : "names synced", privacy: .public)")
            refreshConversations()
        case .messageChanged(let conversationId, let key):
            append("message \(conversationId) \(key.author.prefix(8)) @\(key.sentAt)")
            refreshConversations()
            if conversationId == selectedId {
                changedKeys.insert(key)
                reloadMessages()
            }
        case .typing(let conversationId, let who, let started):
            setTyping(conversationId, who: who, until: started ? now.addingTimeInterval(Self.typingTimeout) : nil)
        case .error(let message):
            // error text can mention ids or numbers: keep it out of the system log
            Self.logger.error("core error: \(message, privacy: .private)")
            lastError = message
            append("error: \(message)")
        }
    }

    // MARK: Open conversation

    /// Opens a conversation and loads its newest page.
    func select(_ id: String?) {
        guard id != selectedId else { return }
        selectedId = id
        selectionGeneration += 1
        messages = []
        reactions = [:]
        changedKeys = []
        hasOlder = false
        replyTo = nil
        reloadMessages()
    }

    /// Prepends the page before the oldest loaded message. Ignored while one is loading.
    func loadOlder() async {
        guard let client, let id = selectedId, hasOlder, !loadingOlder, let oldest = messages.first else { return }
        loadingOlder = true
        defer { loadingOlder = false }
        let generation = selectionGeneration
        do {
            let page = try await client.messages(
                conversationId: id, before: MessageKey(author: oldest.author, sentAt: oldest.sentAt),
                limit: Self.pageSize)
            guard generation == selectionGeneration, messages.first == oldest else { return }
            messages.insert(contentsOf: page, at: 0)
            hasOlder = page.count == Int(Self.pageSize)
            await loadDetails(for: page, generation: generation)
        } catch { append("query failed: \(error)") }
    }

    /// Sends `text` to the open conversation. Returns false if it couldn't be handed to the core
    /// (the caller keeps the text).
    @discardableResult
    func send(_ text: String) async -> Bool {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let client, let id = selectedId, !body.isEmpty else { return false }
        let quote = replyTo.map { Quote(author: $0.author, sentAt: $0.sentAt, text: $0.body) }
        do {
            // the core announces the new pending message itself
            _ = try await client.sendText(conversationId: id, body: body, quote: quote)
            replyTo = nil
            return true
        } catch {
            lastError = Self.describe(error)
            append("send failed: \(error)")
            return false
        }
    }

    func retry(_ key: MessageKey) async {
        guard let client else { return }
        do {
            if try await !client.retry(key: key) { append("retry skipped: the message isn't failed") }
        } catch { append("retry failed: \(error)") }
    }

    func isTyping(_ conversationId: String, now: Date = .now) -> Bool {
        typingUntil[conversationId]?.values.contains { $0 > now } ?? false
    }

    var selectedConversation: Conversation? {
        selectedId.flatMap { id in conversations.first { $0.id == id } }
    }

    /// A conversation's name, or a short form of its id until the name is known.
    nonisolated static func displayTitle(_ c: Conversation) -> String {
        if !c.title.isEmpty { return c.title }
        if c.id.hasPrefix("g:") { return "Group" }
        return String(c.id.dropFirst(2).prefix(8))
    }

    /// A group member's display name: their contact name, else a short id. Never empty.
    func senderName(_ author: String) -> String {
        senderNames[author] ?? String(author.prefix(8))
    }

    private func setTyping(_ conversationId: String, who: String, until: Date?) {
        typingUntil[conversationId, default: [:]][who] = until
        if typingUntil[conversationId]?.isEmpty == true { typingUntil[conversationId] = nil }
        guard let until else { return }
        // clear it when it expires so the views update without another event
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0, until.timeIntervalSinceNow)))
            guard let self, self.typingUntil[conversationId]?[who] == until else { return }
            self.setTyping(conversationId, who: who, until: nil)
        }
    }

    /// Reloads the newest page of the open conversation and merges it with older pages already
    /// loaded. Coalesced like the list refresh; a new selection starts its own load.
    private func reloadMessages() {
        guard let client, let id = selectedId else { return }
        let generation = selectionGeneration
        if loadingGeneration == generation {
            reloadMessagesAgain = true
            return
        }
        loadingGeneration = generation
        reloadMessagesAgain = false
        Task {
            repeat {
                reloadMessagesAgain = false
                let changed = changedKeys
                changedKeys = []
                do {
                    let page = try await client.messages(conversationId: id, before: nil, limit: Self.pageSize)
                    guard generation == selectionGeneration else { return }  // switched chats meanwhile
                    let previous = Set(messages.map { MessageKey(author: $0.author, sentAt: $0.sentAt) })
                    messages = Self.merge(loaded: messages, newest: page, pageSize: Self.pageSize, hasOlder: &hasOlder)
                    // older loaded messages that changed are fetched one by one
                    let pageKeys = Set(page.map { MessageKey(author: $0.author, sentAt: $0.sentAt) })
                    for key in changed.subtracting(pageKeys) {
                        if let fresh = try await client.message(key: key), generation == selectionGeneration,
                            let i = messages.firstIndex(where: { $0.author == key.author && $0.sentAt == key.sentAt })
                        {
                            messages[i] = fresh
                        }
                    }
                    // details only for messages that are new or changed
                    let needDetails = messages.filter { m in
                        let k = MessageKey(author: m.author, sentAt: m.sentAt)
                        return !previous.contains(k) || changed.contains(k)
                    }
                    await loadDetails(for: needDetails, generation: generation)
                } catch { append("query failed: \(error)") }
            } while reloadMessagesAgain && generation == selectionGeneration
            if loadingGeneration == generation { loadingGeneration = nil }
        }
    }

    /// The loaded window after a reload: older loaded messages followed by the newest page. If
    /// the newest page doesn't reach back to what was loaded (a burst of new messages), the
    /// window restarts at the newest page so there is never a gap.
    static func merge(loaded: [Message], newest page: [Message], pageSize: UInt32, hasOlder: inout Bool)
        -> [Message]
    {
        guard let first = page.first else { return loaded.isEmpty ? [] : loaded }
        let older = loaded.filter { ($0.sentAt, $0.author) < (first.sentAt, first.author) }
        let pageIsFull = page.count == Int(pageSize)
        let newestLoaded = loaded.last.map { ($0.sentAt, $0.author) }
        if pageIsFull, let newestLoaded, newestLoaded < (first.sentAt, first.author) {
            hasOlder = true
            return page
        }
        if older.isEmpty { hasOlder = pageIsFull }
        return older + page
    }

    /// Reactions and unknown group-sender names for the given messages.
    private func loadDetails(for page: [Message], generation: Int) async {
        guard let client else { return }
        let isGroup = selectedId?.hasPrefix("g:") == true
        for m in page {
            let key = MessageKey(author: m.author, sentAt: m.sentAt)
            do {
                let r = try await client.reactions(key: key)
                if generation == selectionGeneration { reactions[key] = r.isEmpty ? nil : r }
            } catch { append("query failed: \(error)") }
            if isGroup, !m.isOutgoing, senderNames[m.author] == nil, !unnamedAuthors.contains(m.author) {
                do {
                    if let name = try await client.title(conversationId: "c:\(m.author)") {
                        senderNames[m.author] = name
                    } else {
                        unnamedAuthors.insert(m.author)
                    }
                } catch { append("query failed: \(error)") }
            }
        }
    }

    /// One query at a time; events arriving meanwhile cause exactly one more pass, so a burst
    /// can't flood the core and an older result can never overwrite a newer one.
    private func refreshConversations() {
        guard let client else { return }
        if refreshing {
            refreshAgain = true
            return
        }
        refreshing = true
        Task {
            repeat {
                refreshAgain = false
                do {
                    conversations = try await client.conversations()
                    Self.logger.notice("conversations: \(self.conversations.count, privacy: .public)")
                } catch { append("query failed: \(error)") }
            } while refreshAgain
            refreshing = false
        }
    }

    private func append(_ text: String) {
        log.append(LogLine(id: nextLogId, text: text))
        nextLogId += 1
        if log.count > Self.logLimit { log.removeFirst(log.count - Self.logLimit) }
    }

    private static func describe(_ error: Error) -> String {
        switch error {
        case CoreError.AlreadyOpen:
            return "Semaphore is already running."
        case KeychainError.status(errSecInteractionNotAllowed), KeychainError.status(errSecAuthFailed),
            KeychainError.status(errSecUserCanceled):
            return "Semaphore couldn't read its key from your keychain. Unlock the keychain, allow access, and reopen."
        case CoreError.Storage(let message):
            return "Semaphore couldn't open its data: \(message)"
        default:
            return "\(error)"
        }
    }
}

/// Receives core events on the core thread and hands them to the bridge on the main actor.
final class Listener: EventListener {
    private let deliver: @Sendable (CoreEvent) -> Void

    init(bridge: AppStore) {
        deliver = { [weak bridge] event in
            Task { @MainActor in bridge?.handle(event) }
        }
    }

    func onEvent(event: CoreEvent) { deliver(event) }
}
