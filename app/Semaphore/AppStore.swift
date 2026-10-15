import AppKit
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
    private(set) var conversations: [Conversation] = [] {
        didSet {
            badge = ChatList.badge(conversations)
            NSApp?.dockTile.badgeLabel = badge
            // a chat read here or on the phone takes its message notifications with it
            for c in conversations where c.unreadCount == 0 && notified[c.id] == true {
                removeNotifications(c.id)
            }
        }
    }
    /// The dock badge: total unread.
    private(set) var badge: String?
    /// Our own ACI once linked (Note to Self is `c:<it>`).
    private(set) var ownAci: String?
    /// The chat list's search text; the list and keyboard navigation show only matching chats.
    var searchQuery = ""
    /// Bumped to ask the sidebar to focus the search field.
    private(set) var searchFocusRequest = 0
    /// Set to move focus to the composer (a click on a chat, ↩ in the search, a shortcut); the
    /// composer clears it. Switching chats with the arrows in the list or the search leaves focus
    /// there.
    private(set) var composerWantsFocus = false
    /// Chats whose mark-read call is in flight.
    private var markingRead: Set<String> = []
    /// Chats with notifications showing; true if any is for a message (reactions don't count as
    /// unread, so those go away when the chat is opened).
    private var notified: [String: Bool] = [:]
    /// The notification showing for each message, so a deletion can take it away.
    private var notifiedMessages: [MessageKey: String] = [:]
    /// The chat list has loaded once (actions from notifications need its unread counts).
    private var conversationsLoaded = false
    private let notifier: Notifier?
    private let isAppActive: @MainActor () -> Bool
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

    init(
        open: @escaping Opener = AppStore.openReal, notifier: Notifier? = nil,
        isAppActive: @escaping @MainActor () -> Bool = { AppStore.appIsActive() }
    ) {
        self.open = open
        self.notifier = notifier
        self.isAppActive = isAppActive
    }

    /// In front with the window on screen: then the chat list shows what's new, and notifications
    /// stay quiet (as in Signal Desktop).
    static func appIsActive() -> Bool {
        NSApp.isActive && (NSApp.keyWindow?.occlusionState.contains(.visible) ?? false)
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
                ownAci = client.ownAci()
                notifier?.authorize()
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
            ownAci = client?.ownAci()
            notifier?.authorize()
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
            if notifiedMessages[key] != nil { Task { await removeIfDeleted(key) } }
        case .newMessage(let conversationId, let key):
            Task { await notify(conversationId, about: key, reaction: nil) }
        case .newReaction(let conversationId, let target, let reactor, let emoji):
            Task { await notify(conversationId, about: target, reaction: (reactor, emoji)) }
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
        if let id { removeNotifications(id) }
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

    /// Marks a chat read (here, on the phone, and with read receipts) if it has unread messages.
    func markRead(_ conversationId: String) {
        guard let client, !markingRead.contains(conversationId),
            conversations.first(where: { $0.id == conversationId })?.unreadCount ?? 0 > 0
        else { return }
        markingRead.insert(conversationId)
        Task {
            defer { markingRead.remove(conversationId) }
            do { _ = try await client.markRead(conversationId: conversationId) } catch {
                lastError = "Couldn't mark the chat read: \(Self.describe(error))"
                append("mark read failed: \(error)")
            }
        }
    }

    /// Opens the chat `step` away in the visible list (see `ChatList.neighbor`).
    func selectNeighbor(step: Int, unreadOnly: Bool) {
        if let id = ChatList.neighbor(in: visibleConversations, of: selectedId, step: step, unreadOnly: unreadOnly) {
            select(id)
        }
    }

    /// Opens the chat at 1-based position `n` of the visible list.
    func selectChat(at n: Int) {
        if let id = ChatList.at(n, in: visibleConversations) { select(id) }
    }

    /// ↩ in the search: opens the selected match (or the first), ends the search and moves to the
    /// composer.
    func openSearchResult() {
        let shown = visibleConversations
        let id = shown.contains { $0.id == selectedId } ? selectedId : shown.first?.id
        guard let id else { return }
        select(id)
        searchQuery = ""
        focusComposer()
    }

    // MARK: Notifications

    /// Shows a notification for a new message, or for `reaction` to our message `key`, unless the
    /// app is in front or notifications are off.
    private func notify(_ conversationId: String, about key: MessageKey, reaction: (reactor: String, emoji: String)?)
        async
    {
        guard let notifier, let client, !isAppActive() else { return }
        let setting = NotificationContent.current()
        guard setting != .off else { return }
        do {
            guard let m = try await client.message(key: key), !m.deleted else { return }
            let author = reaction?.reactor ?? m.author
            await loadName(author)
            let groupTitle =
                conversationId.hasPrefix("g:") ? try await client.title(conversationId: conversationId) ?? "Group" : nil
            guard
                let request = Notifications.request(
                    setting: setting, conversationId: conversationId,
                    id: "\(conversationId)|\(author)|\(key.sentAt)|\(reaction?.emoji ?? "")",
                    sender: senderName(author), groupTitle: groupTitle, text: m.body, reaction: reaction?.emoji)
            else { return }
            notifier.post(request)
            notified[conversationId] = notified[conversationId] == true || reaction == nil
            notifiedMessages[key] = request.id
        } catch {
            Self.logger.error("notification failed: \(error, privacy: .private)")
            append("notification failed: \(error)")
        }
    }

    /// A message deleted for everyone must not stay readable in Notification Center.
    private func removeIfDeleted(_ key: MessageKey) async {
        guard let client, let id = notifiedMessages[key] else { return }
        do {
            guard try await client.message(key: key)?.deleted ?? true else { return }
            notifiedMessages[key] = nil
            notifier?.remove(ids: [id])
        } catch { append("query failed: \(error)") }
    }

    private func removeNotifications(_ conversationId: String) {
        notified[conversationId] = nil
        notifier?.remove(conversationId: conversationId)
    }

    /// A click on a notification: brings up its chat, ready to type.
    func openFromNotification(conversationId: String) async {
        guard await ready() else {
            return Self.logger.error("opening a chat from a notification failed: Semaphore wasn't ready")
        }
        select(conversationId)
        focusComposer()
        NSApp?.activate()
    }

    /// Reply typed into a notification: sends it and marks the chat read (the reply shows it was).
    func replyFromNotification(conversationId: String, text: String) async {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty else { return }
        guard await ready(), let client else {
            return replyFailed(conversationId, because: "Semaphore wasn't ready")
        }
        do {
            _ = try await client.sendText(conversationId: conversationId, body: body, quote: nil)
            markRead(conversationId)
        } catch {
            replyFailed(conversationId, because: Self.describe(error))
        }
    }

    /// The app is in the background when a reply comes from a notification: say so the same way.
    private func replyFailed(_ conversationId: String, because reason: String) {
        Self.logger.error("reply from a notification failed: \(reason, privacy: .private)")
        lastError = "Your reply couldn't be sent: \(reason)"
        notifier?.post(
            NotificationRequest(
                id: "reply-failed-\(conversationId)", conversationId: conversationId, title: "Reply not sent",
                body: "Open the chat to send it again.", hasActions: false))
    }

    func markReadFromNotification(conversationId: String) async {
        guard await ready() else {
            return Self.logger.error("mark read from a notification dropped: Semaphore wasn't ready")
        }
        markRead(conversationId)
        removeNotifications(conversationId)
    }

    /// Waits (up to 10 s) for the core to be open and linked and the chat list loaded: a click
    /// on a notification can launch the app.
    private func ready() async -> Bool {
        for _ in 0..<100 {
            if client != nil, screen == .chats, conversationsLoaded { return true }
            try? await Task.sleep(for: .milliseconds(100))
        }
        return false
    }

    func focusSearch() { searchFocusRequest += 1 }
    func focusComposer() { composerWantsFocus = true }
    func composerTookFocus() { composerWantsFocus = false }

    /// Recomputes day separators ("Today" becomes "Yesterday" after midnight).
    func refreshDays() { timelineItems = Timeline.items(for: messages) }

    func isTyping(_ conversationId: String, now: Date = .now) -> Bool {
        typingUntil[conversationId]?.values.contains { $0 > now } ?? false
    }

    var selectedConversation: Conversation? {
        selectedId.flatMap { id in conversations.first { $0.id == id } }
    }

    /// The chats the list shows: all, or those matching the search.
    var visibleConversations: [Conversation] {
        conversations.filter { Formatting.matches(title($0), query: searchQuery) }
    }

    func title(_ c: Conversation) -> String { Self.displayTitle(c, ownAci: ownAci) }

    /// A conversation's name ("Note to Self" for our own), or a short form of its id until the
    /// name is known.
    nonisolated static func displayTitle(_ c: Conversation, ownAci: String? = nil) -> String {
        if let ownAci, c.id == "c:\(ownAci)" { return "Note to Self" }
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
            if isGroup, !m.isOutgoing { await loadName(m.author) }
        }
    }

    /// Looks up a group member's contact name once.
    private func loadName(_ author: String) async {
        guard let client, senderNames[author] == nil, !unnamedAuthors.contains(author) else { return }
        do {
            if let name = try await client.title(conversationId: "c:\(author)") {
                senderNames[author] = name
            } else {
                unnamedAuthors.insert(author)
            }
        } catch { append("query failed: \(error)") }
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
                    conversationsLoaded = true
                    Self.logger.notice("conversations: \(self.conversations.count, privacy: .public)")
                    // group rows say who wrote the last message
                    for c in conversations where c.kind == .group && !c.lastMessageOutgoing {
                        await loadName(c.lastMessageAuthor)
                    }
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
