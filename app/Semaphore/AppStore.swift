import AppKit
import OSLog
import Observation

/// The app's state: linking, the conversation list, the open conversation and its messages.
/// Fed by core events; every query result is checked against the current selection.
@MainActor @Observable
final class AppStore {
    enum Screen: Equatable {
        /// `unlinked`: this Mac was removed on the phone; linking again starts from nothing.
        case starting, link, chats, unlinked
        case failed(String)
    }

    struct LogLine: Identifiable, Equatable {
        let id: Int
        let text: String
        var at = Date()
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
    /// The chat list's search text; the list and keyboard navigation show only matching chats,
    /// and matching messages are looked up (`searchResults`).
    var searchQuery = "" { didSet { if searchQuery != oldValue { scheduleSearch() } } }
    /// Search only in this chat (⇧⌘F), or everywhere.
    private(set) var searchScope: String? { didSet { if searchScope != oldValue { scheduleSearch() } } }
    private(set) var searchResults: [SearchHit] = []
    /// A search for the current query is on its way (results are empty meanwhile, never stale).
    private(set) var searchPending = false
    /// Why the last search failed, if it did.
    private(set) var searchFailed: String?
    private var searchTask: Task<Void, Never>?
    /// Bumped by each `open(hit)`: only the latest one moves the timeline.
    private var openGeneration = 0
    /// A message found by search: highlighted for a moment, and scrolled to (`scrollRequest`).
    private(set) var highlighted: MessageKey?
    private(set) var scrollTarget: MessageKey?
    private(set) var scrollRequest = 0
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
    /// Removes the database key (with the data: unlinking, linking again).
    private let forgetKey: () -> Void
    private(set) var log: [LogLine] = []
    private(set) var lastError: String?
    /// The core's recent errors, for the Core Log window (ids and reasons, never message text).
    private(set) var errors: [LogLine] = []
    private(set) var selectedId: String?
    /// The open conversation's loaded messages, oldest first.
    private(set) var messages: [Message] = [] {
        didSet { timelineItems = Timeline.items(for: messages, events: events) }
    }
    /// The open conversation's events (undecryptable messages…) over the loaded time span.
    private(set) var events: [ChatEvent] = [] {
        didSet { timelineItems = Timeline.items(for: messages, events: events) }
    }
    /// Rows for the timeline, recomputed only when `messages` change.
    private(set) var timelineItems: [Timeline.Item] = []
    private(set) var hasOlder = false
    private(set) var reactions: [MessageKey: [Reaction]] = [:]
    /// Files and link previews of the loaded messages.
    private(set) var attachments: [MessageKey: [Attachment]] = [:]
    private(set) var previews: [MessageKey: LinkPreview] = [:]
    /// Files chosen, dropped or pasted for the open chat, sent with the next message.
    private(set) var outgoingFiles: [OutgoingFile] = []
    var replyTo: Message?
    /// The message of ours being edited in the composer, if any.
    private(set) var editing: Message?
    /// Our reactions sent but not back from the core yet (nil: taken back), so a second click
    /// acts on what we just did.
    private var pendingReactions: [MessageKey: String?] = [:]
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
    /// Which core's events count: bumped when starting over, so a closed core's late events
    /// (or a refresh still running against it) change nothing.
    private(set) var listenerGeneration = 0
    private var nextLogId = 0
    private var refreshing = false
    private var refreshAgain = false

    init(
        open: @escaping Opener = AppStore.openReal, notifier: Notifier? = nil,
        isAppActive: @escaping @MainActor () -> Bool = { AppStore.appIsActive() },
        forgetKey: @escaping () -> Void = { Keychain().delete() }
    ) {
        self.open = open
        self.notifier = notifier
        self.isAppActive = isAppActive
        self.forgetKey = forgetKey
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
        let listener = Listener(bridge: self, generation: listenerGeneration)
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

    /// An event from the core with listener generation `generation`; dropped if that core was
    /// replaced.
    func receive(_ event: CoreEvent, generation: Int) {
        if generation == listenerGeneration { handle(event) }
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
                // removed on the phone while in use; otherwise a linking attempt failed
                screen = screen == .chats ? .unlinked : .link
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
        case .messageExpired(let conversationId, let key):
            // its disappearing timer ran out: nothing of it may stay on screen
            append("expired \(conversationId) \(key.author.prefix(8)) @\(key.sentAt)")
            refreshConversations()
            if conversationId == selectedId {
                messages.removeAll { $0.author == key.author && $0.sentAt == key.sentAt }
            }
            searchResults.removeAll { $0.message.author == key.author && $0.message.sentAt == key.sentAt }
            notifiedMessages[key] = nil
            notifier?.remove(idPrefix: Notifications.idPrefix(conversationId, key))
        case .newMessage(let conversationId, let key):
            Task { await notify(conversationId, about: key, reaction: nil) }
        case .attachmentChanged(let conversationId, let message, _):
            // a download finished (or started, or failed): its message in the open chat updates
            if conversationId == selectedId {
                changedKeys.insert(message)
                reloadMessages()
            }
        case .newReaction(let conversationId, let target, let reactor, let emoji):
            Task { await notify(conversationId, about: target, reaction: (reactor, emoji)) }
        case .typing(let conversationId, let who, let started):
            setTyping(conversationId, who: who, until: started ? now.addingTimeInterval(Self.typingTimeout) : nil)
        case .chatEventAdded(let conversationId):
            refreshConversations()
            if conversationId == selectedId { reloadEvents() }
        case .error(let message):
            // error text can mention ids or numbers: keep it out of the system log
            Self.logger.error("core error: \(message, privacy: .private)")
            lastError = message
            append("error: \(message)")
            errors.append(LogLine(id: nextLogId, text: message))
            if errors.count > Self.logLimit { errors.removeFirst(errors.count - Self.logLimit) }
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
        pendingReactions = [:]
        events = []
        attachments = [:]
        previews = [:]
        outgoingFiles = []
        changedKeys = []
        hasOlder = false
        replyTo = nil
        editing = nil
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
            reloadEvents()
            await loadDetails(for: page, generation: generation)
        } catch { append("query failed: \(error)") }
    }

    /// Sends `text` to the open conversation. Returns false if it couldn't be handed to the core
    /// (the caller keeps the text).
    @discardableResult
    func send(_ text: String) async -> Bool {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if let editing { return await saveEdit(of: editing, body) }
        let files = outgoingFiles
        guard let client, let id = selectedId, !body.isEmpty || !files.isEmpty else { return false }
        // taken out of the tray now, so a second ↩ while this one is busy can't send them again
        outgoingFiles = []
        let quote = replyTo.map { Quote(author: $0.author, sentAt: $0.sentAt, text: $0.body) }
        do {
            // the core announces the new pending message itself
            if files.isEmpty {
                _ = try await client.sendText(conversationId: id, body: body, quote: quote)
            } else {
                // the text goes with the files, as their caption
                _ = try await client.sendAttachments(
                    conversationId: id, files: files, caption: body.isEmpty ? nil : body, quote: quote)
            }
            replyTo = nil
            return true
        } catch {
            // back in the tray, unless the chat changed meanwhile
            if selectedId == id { outgoingFiles = files + outgoingFiles }
            lastError = Self.describe(error)
            append("send failed: \(error)")
            return false
        }
    }

    /// Something the user tried didn't work; the reason shows in the window.
    func showError(_ message: String) {
        Self.logger.error("\(message, privacy: .private)")
        lastError = message
    }

    // MARK: Reactions, edits, deletes

    /// Reacts to a message: our current reaction again takes it back, another replaces it.
    func react(to m: Message, with emoji: String) async {
        guard let client else { return }
        let key = MessageKey(author: m.author, sentAt: m.sentAt)
        let (emoji, remove) = MessageActions.reaction(choosing: emoji, mine: myReaction(to: m))
        do {
            try await client.react(target: key, emoji: emoji, remove: remove)
            pendingReactions[key] = .some(remove ? nil : emoji)
        } catch {
            showError("Couldn't react: \(Self.describe(error))")
        }
    }

    /// Our reaction to a message, counting one just sent.
    func myReaction(to m: Message) -> String? {
        let key = MessageKey(author: m.author, sentAt: m.sentAt)
        if let pending = pendingReactions[key] { return pending }
        return MessageActions.mine(reactions[key] ?? [], me: ownAci)
    }

    /// Replies to a message (ends editing: the two don't mix).
    func reply(to m: Message) {
        editing = nil
        replyTo = m
    }

    /// Puts one of our messages in the composer to edit it.
    func startEditing(_ m: Message) {
        replyTo = nil
        editing = m
        focusComposer()
    }

    func cancelEditing() { editing = nil }

    /// ↑ in an empty composer: edits our newest message that can still be edited. False if none.
    func editLast() -> Bool {
        guard let m = MessageActions.lastEditable(messages, me: ownAci) else { return false }
        startEditing(m)
        return true
    }

    private func saveEdit(of m: Message, _ body: String) async -> Bool {
        guard let client, !body.isEmpty else { return false }
        do {
            try await client.edit(key: MessageKey(author: m.author, sentAt: m.sentAt), newBody: body)
            editing = nil
            return true
        } catch {
            showError("Couldn't edit the message: \(Self.describe(error))")
            return false
        }
    }

    func deleteForEveryone(_ m: Message) async {
        guard let client else { return }
        do { try await client.deleteForEveryone(key: MessageKey(author: m.author, sentAt: m.sentAt)) } catch {
            showError("Couldn't delete the message: \(Self.describe(error))")
        }
    }

    /// Adds files to the tray of the open chat (each once).
    func addFiles(_ files: [OutgoingFile]) {
        // an edit changes the text only (as in Desktop)
        guard selectedId != nil, editing == nil else { return }
        for f in files where !outgoingFiles.contains(where: { $0.path == f.path }) { outgoingFiles.append(f) }
    }

    func removeFile(_ path: String) { outgoingFiles.removeAll { $0.path == path } }

    /// Downloads a file that didn't download by itself, or failed.
    func download(_ id: Int64) {
        do { try client?.downloadAttachment(id: id) } catch {
            lastError = Self.describe(error)
            append("download failed: \(error)")
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
        // searching a chat, or no chat matches: the newest message found
        if searchScope != nil || visibleConversations.isEmpty {
            if let hit = searchResults.first { Task { await open(hit) } }
            return
        }
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
            // a message with files reads "📷 Photo" (or "📷 caption"), as in Desktop
            var text = Placeholder.text(m.kind) ?? m.body
            do {
                if let first = try await client.attachments(key: key).first {
                    text = Files.describe(Files.summary(first), body: m.body)
                }
            } catch { append("query failed: \(error)") }  // still announced, with its text
            let author = reaction?.reactor ?? m.author
            await loadName(author)
            let groupTitle =
                conversationId.hasPrefix("g:") ? try await client.title(conversationId: conversationId) ?? "Group" : nil
            guard
                let request = Notifications.request(
                    setting: setting, conversationId: conversationId,
                    id: Notifications.id(conversationId, key, reactor: reaction?.reactor, emoji: reaction?.emoji),
                    sender: senderName(author), groupTitle: groupTitle, text: text, reaction: reaction?.emoji)
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

    // MARK: Connection and linking

    /// "Retry" on the connection banner: reconnects now.
    func retryConnection() {
        do {
            // receiving may have stopped (it is restarted only on request); starting is idempotent
            try client?.start()
            try client?.reconnect()
        } catch { showError("Couldn't reconnect: \(Self.describe(error))") }
    }

    /// The network came back: reconnect at once rather than after the backoff.
    func networkReturned() {
        if screen == .chats { retryConnection() }
    }

    /// Starts a new secure session with a contact whose messages couldn't be decrypted.
    func resetSession(_ conversationId: String) {
        do { try client?.resetSession(conversationId: conversationId) } catch {
            showError("Couldn't reset the session: \(Self.describe(error))")
        }
    }

    /// After the phone removed this Mac: removes what is left of the old link and shows a new
    /// code.
    func linkAgain() async {
        if let failed = await unlinkThisMac(tellServer: false) { showError(failed) }
    }

    /// Unlinks this Mac and removes all its data, telling the server first if `tellServer`.
    /// Returns why the server couldn't be told (nothing is removed then: the caller can offer to
    /// unlink anyway), or nil: done and a new linking code is on its way, or the core stopped but
    /// its data couldn't all be removed (the failed screen says so).
    func unlinkThisMac(tellServer: Bool) async -> String? {
        guard let client else { return "Semaphore isn't running." }
        do {
            try await client.unlink(tellServer: tellServer)
        } catch CoreError.Network(let message) {
            Self.logger.error("unlink failed: \(message, privacy: .private)")
            return Self.describe(CoreError.Network(message: message))
        } catch {
            // the core has stopped: there is no going back to the chats
            Self.logger.error("removing the data failed: \(Self.describe(error), privacy: .private)")
            screen = .failed(
                "Semaphore couldn't remove its data (\(Self.describe(error))). Quit Semaphore, delete "
                    + "~/Library/Application Support/Semaphore, and open it again.")
            return nil
        }
        await restart()
        return nil
    }

    /// Starts over with a fresh, unlinked core: everything shown is forgotten, and the key goes
    /// with the data it protected.
    private func restart() async {
        listenerGeneration += 1
        client = nil
        forgetKey()
        notifier?.removeAll()  // message previews go with the data
        refreshing = false
        refreshAgain = false
        conversations = []
        select(nil)
        searchQuery = ""
        searchScope = nil
        typingUntil = [:]
        senderNames = [:]
        unnamedAuthors = []
        notified = [:]
        notifiedMessages = [:]
        conversationsLoaded = false
        ownAci = nil
        linkURL = nil
        lastError = nil
        connection = .offline
        screen = .starting
        booted = false
        await boot()
    }

    /// The open conversation's events since its oldest loaded message (all if it is loaded
    /// from the start).
    private func reloadEvents() {
        guard let client, let id = selectedId else { return }
        let generation = selectionGeneration
        let since = hasOlder ? messages.first?.sentAt ?? 0 : 0
        Task {
            do {
                let found = try await client.chatEvents(conversationId: id, since: since)
                if generation == selectionGeneration { events = found }
            } catch { append("query failed: \(error)") }
        }
    }

    // MARK: Search

    static let searchLimit: UInt32 = 50
    /// How far back a search result can be shown: the open chat is loaded up to it.
    static let jumpLimit: UInt32 = 10_000

    /// ⇧⌘F: search the open chat only.
    func searchInChat() {
        guard let id = selectedId else { return }
        searchScope = id
        focusSearch()
    }

    func endSearch() {
        searchQuery = ""
        searchScope = nil
    }

    /// Looks the query up once typing pauses; a newer query cancels an older one.
    private func scheduleSearch() {
        searchTask?.cancel()
        // results of an older query must never be shown (or opened) under this one
        searchResults = []
        searchFailed = nil
        let query = searchQuery.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let client, !query.isEmpty else {
            searchPending = false
            return
        }
        searchPending = true
        let scope = searchScope
        searchTask = Task {
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            do {
                let hits = try await client.search(query: query, conversationId: scope, limit: Self.searchLimit)
                guard !Task.isCancelled else { return }
                searchResults = hits
            } catch {
                guard !Task.isCancelled else { return }
                Self.logger.error("search failed: \(error, privacy: .private)")
                searchFailed = Self.describe(error)
            }
            searchPending = false
        }
    }

    /// Opens a search result: its chat, loaded back to the message, scrolled to and highlighted.
    func open(_ hit: SearchHit) async {
        let key = MessageKey(author: hit.message.author, sentAt: hit.message.sentAt)
        let conversationId = hit.message.conversationId
        select(conversationId)
        guard let client else { return }
        openGeneration += 1
        let (generation, opening) = (selectionGeneration, openGeneration)
        // a newer open (arrowing on through results) or another chat makes this one moot
        let current = { generation == self.selectionGeneration && opening == self.openGeneration }
        var page: [Message] = []
        do {
            let newer = try await client.countNewer(conversationId: conversationId, key: key)
            guard current() else { return }
            guard newer < Self.jumpLimit else {
                return showError("That message is too far back to show yet.")
            }
            let limit = max(Self.pageSize, newer + 50)
            page = try await client.messages(conversationId: conversationId, before: nil, limit: limit)
            guard current() else { return }
            if page.count > messages.count {
                messages = page
                hasOlder = page.count == Int(limit)
            }
        } catch {
            return showError("Couldn't open that message: \(Self.describe(error))")
        }
        // to the message at once; details (reactions, files) follow, those around it first
        highlighted = key
        scrollTarget = key
        scrollRequest += 1
        Task {
            try? await Task.sleep(for: .seconds(2))
            if highlighted == key { highlighted = nil }
        }
        let at = page.firstIndex { $0.author == key.author && $0.sentAt == key.sentAt } ?? page.count
        let near = Array(page[max(0, at - 30)..<min(page.count, at + 30)])
        let nearKeys = Set(near.map { MessageKey(author: $0.author, sentAt: $0.sentAt) })
        await loadDetails(for: near, generation: generation)
        guard current() else { return }
        await loadDetails(
            for: page.filter { !nearKeys.contains(MessageKey(author: $0.author, sentAt: $0.sentAt)) },
            generation: generation)
    }
    func focusComposer() { composerWantsFocus = true }
    func composerTookFocus() { composerWantsFocus = false }

    /// Recomputes day separators ("Today" becomes "Yesterday" after midnight).
    func refreshDays() { timelineItems = Timeline.items(for: messages, events: events) }

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
                        let fresh = try await client.message(key: key)
                        guard generation == selectionGeneration,
                            let i = messages.firstIndex(where: { $0.author == key.author && $0.sentAt == key.sentAt })
                        else { continue }
                        // gone: its disappearing timer ran out
                        if let fresh { messages[i] = fresh } else { messages.remove(at: i) }
                    }
                    // details only for messages that are new or changed
                    let needDetails = messages.filter { m in
                        let k = MessageKey(author: m.author, sentAt: m.sentAt)
                        return !previous.contains(k) || changed.contains(k)
                    }
                    await loadDetails(for: needDetails, generation: generation)
                } catch { append("query failed: \(error)") }
                // the message being edited was deleted (here or on the phone): nothing left to edit
                if let e = editing,
                    messages.contains(where: { $0.author == e.author && $0.sentAt == e.sentAt && $0.deleted })
                {
                    editing = nil
                }
            } while reloadMessagesAgain && generation == selectionGeneration
            if generation == selectionGeneration { reloadEvents() }
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
            // separately: one failing query doesn't take the others' details with it
            do {
                let r = try await client.reactions(key: key)
                if generation == selectionGeneration {
                    reactions[key] = r.isEmpty ? nil : r
                    pendingReactions[key] = nil  // the core's word now
                }
            } catch { append("query failed: \(error)") }
            do {
                let files = try await client.attachments(key: key)
                if generation == selectionGeneration { attachments[key] = files.isEmpty ? nil : files }
            } catch { append("query failed: \(error)") }
            do {
                let preview = try await client.linkPreview(key: key)
                if generation == selectionGeneration { previews[key] = preview }
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
        let generation = listenerGeneration
        Task {
            // a refresh of a core that was replaced (starting over) leaves everything, flags too,
            // to the new one
            repeat {
                guard generation == listenerGeneration else { return }
                refreshAgain = false
                do {
                    let found = try await client.conversations()
                    guard generation == listenerGeneration else { return }
                    conversations = found
                    conversationsLoaded = true
                    Self.logger.notice("conversations: \(self.conversations.count, privacy: .public)")
                    // group rows say who wrote the last message
                    for c in conversations where c.kind == .group && !c.lastMessageOutgoing {
                        await loadName(c.lastMessageAuthor)
                    }
                } catch { append("query failed: \(error)") }
            } while refreshAgain
            if generation == listenerGeneration { refreshing = false }
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
        case CoreError.InvalidInput(let message):
            return message
        case CoreError.Network(let message):
            return "The server couldn't be reached: \(message)"
        default:
            return "\(error)"
        }
    }
}

/// Receives core events on the core thread and hands them to the bridge on the main actor.
final class Listener: EventListener {
    private let deliver: @Sendable (CoreEvent) -> Void

    init(bridge: AppStore, generation: Int) {
        deliver = { [weak bridge] event in
            Task { @MainActor in bridge?.receive(event, generation: generation) }
        }
    }

    func onEvent(event: CoreEvent) { deliver(event) }
}
