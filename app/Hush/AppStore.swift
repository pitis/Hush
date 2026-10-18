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
    static let deviceName = "Hush"
    /// App state for Console.app (subsystem dev.pitis.hush). Never URLs or message content.
    private static let logger = Logger(subsystem: "dev.pitis.hush", category: "app")

    /// The one active (or just-ended) call, driving the call window.
    struct ActiveCall: Equatable {
        var conversationId: String
        var title: String
        var phase: CallPhase
        var incoming: Bool
        /// Set on the first `.connected`; drives the duration ticker.
        var connectedAt: Date?
        var muted: Bool = false
        /// Why it ended, shown briefly.
        var endedReason: CallEnd?
    }

    /// How long a hang-up may take to come back as `Ended` before the window is forced closed
    /// (a dead engine must not leave a call you can't end).
    static var hangupGrace: Duration = .seconds(5)
    /// How long the ended state stays on screen.
    static var endedLinger: Duration = .seconds(3)

    /// The history transfer's state, for the overlay over the chats.
    struct HistoryProgress: Equatable {
        var stage: HistoryStage
        var done: UInt64
        var total: UInt64
    }

    private(set) var screen: Screen = .starting
    private(set) var linkURL: String?
    /// Non-nil while the phone's message history is on its way (or failed): the overlay shows.
    private(set) var history: HistoryProgress?
    /// Non-nil while a call rings or runs (or just ended): the call window shows it.
    private(set) var call: ActiveCall?
    private(set) var connection: ConnectionState = .offline
    private(set) var conversations: [Conversation] = [] {
        didSet {
            badge = ChatList.badge(conversations, now: Date())
            NSApp?.dockTile.badgeLabel = badge
            recountWhenAMuteEnds()
            // an archive that emptied (the last chat unarchived, or written in) closes
            if showingArchive, ChatList.archived(conversations).isEmpty { showingArchive = false }
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
    /// How restoring a backup went, on the link screen after it (the old one is gone).
    private(set) var restoreNote: String?
    /// Chat settings changed here are written to the phone's account (the Storage Service).
    private(set) var storageWrites = true
    /// The core's recent errors, for the Core Log window (ids and reasons, never message text).
    private(set) var errors: [LogLine] = []
    private(set) var selectedId: String?
    /// A group link clicked in a message, waiting for its join sheet (it holds the group's key:
    /// never logged).
    var joiningLink: String?
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
    /// The link preview for the composer's text, to go with the next message.
    private(set) var previewDraft: PreviewDraft?
    /// The link `previewDraft` is (or will be) for.
    private var previewLink: String?
    private var fetchingPreview: String?
    /// Links already asked about (or dismissed) in this chat: not fetched again.
    private var dismissedPreviews: Set<String> = []
    /// The stories of the last day (see `Client.stories`).
    private(set) var stories: [Story] = []
    /// The polls of the open chat's loaded messages, as their votes stand.
    private(set) var polls: [MessageKey: Poll] = [:]
    /// Files chosen, dropped or pasted for the open chat, sent with the next message.
    private(set) var outgoingFiles: [OutgoingFile] = []
    var replyTo: Message?
    /// The installed sticker packs, as the picker last loaded them.
    var stickerPacks: [StickerPackInfo] = []
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

    /// `~/Library/Application Support/Hush`. Changing it orphans the user's data.
    nonisolated static func dataDirectory() throws -> URL {
        try dataDirectory(
            in: FileManager.default.url(
                for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true))
    }

    /// The folder left by the app's old name is moved here once.
    nonisolated static func dataDirectory(in support: URL) throws -> URL {
        let dir = support.appendingPathComponent("Hush", isDirectory: true)
        let legacy = support.appendingPathComponent(Legacy.name, isDirectory: true)
        let files = FileManager.default
        if !files.fileExists(atPath: dir.path), files.fileExists(atPath: legacy.path) {
            try Legacy.whileClosed(legacy) { try files.moveItem(at: legacy, to: dir) }
        }
        return dir
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
                // before start(): a fast import could otherwise finish in between, and the
                // overlay set afterwards would have no event left to dismiss it
                let pending = (try? await client.historyPending()) == true
                try client.start()
                refreshConversations()
                do { storageWrites = try await client.storageWrites() } catch {
                    append("couldn't read the chat settings sync switch: \(error)")
                }
                if pending, history == nil {
                    // resuming an interrupted transfer: show it before its first event arrives
                    history = HistoryProgress(stage: .waiting, done: 0, total: 0)
                }
                notifier?.remove(idPrefix: "call|")  // a ring from a previous run can't be answered
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
            Self.logger.notice("\(event == .ready ? "caught up" : "chat list changed", privacy: .public)")
            refreshConversations()
            Task { await refreshStories() }
        case .storiesChanged:
            Task { await refreshStories() }
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
            // a story's file, maybe
            if stories.contains(where: { $0.author == message.author && $0.sentAt == message.sentAt }) {
                Task { await refreshStories() }
            }
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
        case .callChanged(let conversationId, let phase, let reason):
            Self.logger.notice("call \(String(describing: phase), privacy: .public)")
            callChanged(conversationId, phase: phase, reason: reason, now: now)
        case .historyProgress(let stage, let done, let total):
            Self.logger.notice("history \(String(describing: stage), privacy: .public)")
            switch stage {
            case .done:
                history = nil
                refreshConversations()
            case .noHistory:
                history = nil
            default:
                history = HistoryProgress(stage: stage, done: done, total: total)
            }
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
        polls = [:]
        previewDraft = nil
        previewLink = nil
        dismissedPreviews = []
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

    /// Sends `text` (already trimmed when it has `ranges`, its styles) to the open conversation.
    /// Returns false if it couldn't be handed to the core (the caller keeps the text).
    @discardableResult
    func send(_ text: String, ranges: [BodyRange] = []) async -> Bool {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if let editing { return await saveEdit(of: editing, body, ranges: ranges) }
        let files = outgoingFiles
        guard let client, let id = selectedId, !body.isEmpty || !files.isEmpty else { return false }
        // taken out of the tray now, so a second ↩ while this one is busy can't send them again
        outgoingFiles = []
        let quote = replyTo.map { Quote(author: $0.author, sentAt: $0.sentAt, text: $0.body, ranges: $0.ranges) }
        // only a preview of a link still in the text
        let preview = previewDraft.flatMap { body.contains($0.url) ? $0 : nil }
        do {
            // the core announces the new pending message itself
            if files.isEmpty {
                _ = try await client.sendText(
                    conversationId: id, body: body, ranges: ranges, quote: quote, preview: preview)
                previewDraft = nil
                previewLink = nil
            } else {
                // the text goes with the files, as their caption
                _ = try await client.sendAttachments(
                    conversationId: id, files: files, caption: body.isEmpty ? nil : body, ranges: ranges, quote: quote)
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

    /// The composer's text changed (the composer waits for a pause in typing): fetches a preview
    /// for its first https link, and drops one whose link is gone. A preview is extra: a failed
    /// fetch shows nothing.
    func composerChanged(_ text: String) async {
        guard editing == nil else { return }
        let link = PreviewLink.first(in: text)
        if link != previewLink {
            previewDraft = nil
            previewLink = link
        }
        guard let link, previewDraft == nil, fetchingPreview != link, !dismissedPreviews.contains(link), let client
        else { return }
        fetchingPreview = link
        // asked once per link in a chat: a page without a preview isn't fetched at every pause
        dismissedPreviews.insert(link)
        defer { if fetchingPreview == link { fetchingPreview = nil } }
        do {
            let draft = try await client.linkPreviewFor(text: text)
            if previewLink == link { previewDraft = draft }
        } catch { append("link preview failed: \(error)") }
    }

    /// ✕ on the preview: none for that link in this chat.
    func dismissPreview() {
        if let previewLink { dismissedPreviews.insert(previewLink) }
        previewDraft = nil
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

    private func saveEdit(of m: Message, _ body: String, ranges: [BodyRange]) async -> Bool {
        guard let client, !body.isEmpty else { return false }
        do {
            try await client.edit(key: MessageKey(author: m.author, sentAt: m.sentAt), newBody: body, ranges: ranges)
            editing = nil
            return true
        } catch {
            showError("Couldn't edit the message: \(Self.describe(error))")
            return false
        }
    }

    /// View-once media was just opened (its file is already linked for the viewer): the core
    /// drops it and tells our phone.
    func viewOnceOpened(_ m: Message) async {
        guard let client else { return }
        do { try await client.viewOnceOpened(key: MessageKey(author: m.author, sentAt: m.sentAt)) } catch {
            showError("Couldn't mark the view-once media opened: \(Self.describe(error))")
        }
    }

    func refreshStories() async {
        guard let client else { return }
        do {
            stories = try await client.stories()
            for author in Set(stories.map(\.author)) where author != ownAci { await loadName(author) }
        } catch { append("query failed: \(error)") }
    }

    /// A story was shown: seen here from now on.
    func storyViewed(_ story: Story) async {
        guard let client else { return }
        do {
            try await client.storyViewed(key: MessageKey(author: story.author, sentAt: story.sentAt))
            if let i = stories.firstIndex(where: { $0.author == story.author && $0.sentAt == story.sentAt }) {
                stories[i].viewed = true
            }
        } catch { append("couldn't mark a story seen: \(error)") }
    }

    /// Finds who has `username` and opens the chat with them. Returns why it didn't, or nil.
    func findUsername(_ username: String) async -> String? {
        guard let client else { return "Not linked yet." }
        do {
            guard let id = try await client.findUsername(username: username) else {
                return "No one has this username."
            }
            select(id)
            refreshConversations()
            return nil
        } catch {
            return Self.describe(error)
        }
    }

    func loadStickerPacks() async {
        guard let client else { return }
        do {
            stickerPacks = try await client.stickerPacks()
        } catch {
            append("sticker packs failed: \(error)")
        }
    }

    /// Sends a sticker to the open chat, as a reply if one is set.
    func sendSticker(pack: String, sticker: UInt32) async {
        guard let client, let id = selectedId else { return }
        let quote = replyTo.map { Quote(author: $0.author, sentAt: $0.sentAt, text: $0.body, ranges: $0.ranges) }
        do {
            // the core announces the new pending message itself
            _ = try await client.sendSticker(conversationId: id, packId: pack, stickerId: sticker, quote: quote)
            replyTo = nil
        } catch {
            lastError = Self.describe(error)
            append("send failed: \(error)")
        }
    }

    /// Adds the pack a sticker link names. Returns why it couldn't, or nil.
    func addStickerPack(_ link: String) async -> String? {
        guard let client else { return "Not linked yet." }
        do {
            try await client.addStickerPack(link: link)
            await loadStickerPacks()
            return nil
        } catch {
            return Self.describe(error)
        }
    }

    /// Votes on a poll (none takes our vote back); it shows once sent.
    func vote(_ m: Message, options: [UInt32]) async {
        guard let client else { return }
        do { try await client.vote(key: MessageKey(author: m.author, sentAt: m.sentAt), options: options) } catch {
            showError("Couldn't vote: \(Self.describe(error))")
        }
    }

    /// Ends our own poll.
    func endPoll(_ m: Message) async {
        guard let client else { return }
        do { try await client.endPoll(key: MessageKey(author: m.author, sentAt: m.sentAt)) } catch {
            showError("Couldn't end the poll: \(Self.describe(error))")
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

    /// Marks a chat read (here, on the phone, and with read receipts) if it has unread messages
    /// or was marked unread.
    func markRead(_ conversationId: String) {
        guard let client, !markingRead.contains(conversationId),
            conversations.first(where: { $0.id == conversationId }).map(ChatList.isUnread) == true
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

    func setArchived(_ conversationId: String, _ archived: Bool) {
        change(conversationId, archived ? "archive the chat" : "unarchive the chat") {
            try await $0.setArchived(conversationId: conversationId, archived: archived)
        }
    }

    /// Sets our bubbles' colour in a chat (nil: the default).
    func setChatColor(_ conversationId: String, _ color: ChatColor?) {
        change(conversationId, "set the chat colour") {
            try await $0.setChatColor(conversationId: conversationId, color: color)
        }
    }

    /// Sets a chat's wallpaper (nil: the default).
    func setWallpaper(_ conversationId: String, _ wallpaper: ChatColor?) {
        change(conversationId, "set the wallpaper") {
            try await $0.setWallpaper(conversationId: conversationId, wallpaper: wallpaper)
        }
    }

    /// Sets a direct chat's disappearing-message timer (0: off).
    func setTimer(_ conversationId: String, seconds: UInt32) {
        change(conversationId, "set the disappearing timer") {
            try await $0.setDisappearingTimer(conversationId: conversationId, seconds: seconds)
        }
    }

    func setPinned(_ conversationId: String, _ pinned: Bool) {
        change(conversationId, pinned ? "pin the chat" : "unpin the chat") {
            try await $0.setPinned(conversationId: conversationId, pinned: pinned)
        }
    }

    func mute(_ conversationId: String, for option: ChatList.MuteOption) {
        let until = option.until(now: Date())
        change(conversationId, "mute the chat") {
            try await $0.setMuteUntil(conversationId: conversationId, untilMs: until)
        }
    }

    func unmute(_ conversationId: String) {
        change(conversationId, "unmute the chat") {
            try await $0.setMuteUntil(conversationId: conversationId, untilMs: 0)
        }
    }

    func setMarkedUnread(_ conversationId: String, _ marked: Bool) {
        // the open chat would count as read again at once: it closes, as on Desktop
        if marked, selectedId == conversationId { select(nil) }
        change(conversationId, marked ? "mark the chat unread" : "mark the chat read") {
            try await $0.setMarkedUnread(conversationId: conversationId, marked: marked)
        }
    }

    /// Nothing the chat sends shows from now on (a group isn't left); the phone follows.
    func block(_ conversationId: String) {
        change(conversationId, "block the chat") { try await $0.block(conversationId: conversationId) }
    }

    /// Accepts the chat's message request, or unblocks it.
    func acceptRequest(_ conversationId: String) {
        change(conversationId, "accept the chat") { try await $0.acceptRequest(conversationId: conversationId) }
    }

    /// Deletes the chat's messages here and, for a person, on the phone too. It closes.
    func deleteChat(_ conversationId: String) {
        if selectedId == conversationId { select(nil) }
        change(conversationId, "delete the chat") { try await $0.deleteChat(conversationId: conversationId) }
    }

    /// The chat flag changes run one after the other, so two quick ones land in order.
    private var lastChange: Task<Void, Never>?
    /// The failure on show, if the last change failed (the next one that works takes it away).
    private var changeFailure: String?

    /// A chat flag change: the core announces the list when it is done; a failure is shown.
    private func change(
        _ conversationId: String, _ action: String, _ call: @escaping (any ClientProtocol) async throws -> Void
    ) {
        guard let client else { return }
        let previous = lastChange
        lastChange = Task {
            await previous?.value
            do {
                try await call(client)
                if let shown = changeFailure, lastError == shown { lastError = nil }
                changeFailure = nil
            } catch {
                changeFailure = "Couldn't \(action): \(Self.describe(error))"
                lastError = changeFailure
                append("couldn't \(action) \(conversationId): \(error)")
            }
        }
    }

    /// Turns writing chat settings to the phone's account on or off; a refusal puts it back.
    func setStorageWrites(_ on: Bool) {
        guard let client, on != storageWrites else { return }
        storageWrites = on
        Task {
            do { try await client.setStorageWrites(on: on) } catch {
                storageWrites = !on
                lastError = "Couldn't change chat settings sync: \(Self.describe(error))"
                append("couldn't turn chat settings sync \(on ? "on" : "off"): \(error)")
            }
        }
    }

    func dismissError() { lastError = nil }

    private var muteEndTask: Task<Void, Never>?

    /// A timed mute that runs out changes the badge and the row's bell without any event: the
    /// list is re-read then.
    private func recountWhenAMuteEnds() {
        muteEndTask?.cancel()
        let nowMs = Int64(Date().timeIntervalSince1970 * 1000)
        guard let next = conversations.map(\.muteUntil).filter({ $0 > nowMs && $0 < Int64.max }).min() else {
            return
        }
        muteEndTask = Task {
            try? await Task.sleep(for: .milliseconds(next - nowMs + 1))
            guard !Task.isCancelled else { return }
            conversations = conversations
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
            // asked of the core now: the list here can lag a mute by a moment
            if try await client.isMuted(conversationId: conversationId) { return }
            guard let m = try await client.message(key: key), !m.deleted else { return }
            // a message with files reads "📷 Photo" (or "📷 caption"), as in Desktop
            for aci in MessageText.mentioned(m.ranges) { await loadName(aci) }
            let body = m.body.map { MessageText.plain($0, ranges: m.ranges, name: mentionName) }
            var text = Placeholder.text(m.kind) ?? body
            do {
                if let first = try await client.attachments(key: key).first {
                    text = Files.describe(Files.summary(first), body: body)
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
            return Self.logger.error("opening a chat from a notification failed: Hush wasn't ready")
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
            return replyFailed(conversationId, because: "Hush wasn't ready")
        }
        do {
            _ = try await client.sendText(
                conversationId: conversationId, body: body, ranges: [], quote: nil, preview: nil)
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
            return Self.logger.error("mark read from a notification dropped: Hush wasn't ready")
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

    // MARK: Calls

    /// A `CallChanged` event: keeps `call` current, rings, and notes missed calls.
    private func callChanged(_ conversationId: String, phase: CallPhase, reason: CallEnd?, now: Date) {
        let title =
            conversations.first { $0.id == conversationId }?.title
            ?? (call?.conversationId == conversationId ? call?.title : nil)
            ?? "Call"
        switch phase {
        case .ended:
            notifier?.remove(ids: [Notifications.callId(conversationId)])
            // an end belongs to the call that is up, not to a stale one
            guard var ended = call, ended.conversationId == conversationId, ended.phase != .ended else { return }
            // a call we never took that the caller gave up on; not one we declined, nor one
            // our phone took
            let missed = ended.incoming && ended.connectedAt == nil && reason.map(Self.isMissed) == true
            if missed,
                let notification = Notifications.missedCall(
                    setting: NotificationContent.current(), conversationId: conversationId, title: title, at: now)
            {
                notifier?.post(notification)
            }
            ended.phase = .ended
            ended.endedReason = reason
            call = ended
            lingerEndedCall()
        case .incoming, .outgoing:
            // a fresh call: an ended (or different) one must not lend it its state
            let fresh =
                call.map { $0.conversationId != conversationId || $0.phase == .ended } ?? true
            if fresh {
                call = ActiveCall(
                    conversationId: conversationId, title: title, phase: phase, incoming: phase == .incoming,
                    connectedAt: nil)
            } else {
                call?.phase = phase
            }
            WindowOpener.open?(id: "call")
            if phase == .incoming, !isAppActive(),
                let ring = Notifications.incomingCall(
                    setting: NotificationContent.current(), conversationId: conversationId, title: title)
            {
                notifier?.post(ring)
            }
        case .ringing, .connecting, .connected:
            guard call?.conversationId == conversationId, call?.phase != .ended else { return }
            call?.phase = phase
            if phase == .connected {
                notifier?.remove(ids: [Notifications.callId(conversationId)])
                if call?.connectedAt == nil { call?.connectedAt = now }
            }
        }
    }

    /// The caller gave up, or nobody could answer: a missed call worth a notification.
    private static func isMissed(_ reason: CallEnd) -> Bool {
        switch reason {
        case .remoteHungUp, .noAnswer, .failed: true
        case .hungUp, .answeredElsewhere, .declinedElsewhere, .busy: false
        }
    }

    /// Starts a voice call in a direct chat (the call window opens on the event).
    func startCall(_ conversationId: String) {
        guard call == nil || call?.phase == .ended else { return showError("You're already in a call.") }
        guard let client else { return showError("Hush isn't running.") }
        do { try client.startCall(conversationId: conversationId) } catch {
            return showError("Couldn't start the call: \(Self.describe(error))")
        }
        // shown at once, so a second click can't start a second call before the core answers
        let title = conversations.first { $0.id == conversationId }?.title ?? "Call"
        call = ActiveCall(
            conversationId: conversationId, title: title, phase: .outgoing, incoming: false, connectedAt: nil)
        WindowOpener.open?(id: "call")
    }

    func acceptCall() {
        guard call != nil, call?.phase != .ended else { return showError("That call is over.") }
        guard let client else { return showError("Hush isn't running.") }
        do { try client.acceptCall() } catch { showError("Couldn't accept the call: \(Self.describe(error))") }
    }

    /// Hangs up — or declines, while it still rings. If the core doesn't confirm within
    /// `hangupGrace`, the window is ended anyway: a stuck engine must not hold the user.
    func hangupCall() {
        guard let current = call, current.phase != .ended else { return }
        notifier?.remove(ids: [Notifications.callId(current.conversationId)])
        guard let client else { return endCallLocally(current.conversationId) }
        do { try client.hangupCall() } catch {
            showError("Couldn't hang up: \(Self.describe(error))")
            return endCallLocally(current.conversationId)
        }
        Task {
            try? await Task.sleep(for: Self.hangupGrace)
            if call?.conversationId == current.conversationId, call?.phase != .ended {
                showError("The call didn't end; the call engine may have stopped.")
                endCallLocally(current.conversationId)
            }
        }
    }

    private func endCallLocally(_ conversationId: String) {
        guard call?.conversationId == conversationId else { return }
        call?.phase = .ended
        call?.endedReason = .failed
        lingerEndedCall()
    }

    /// The ended state stays `endedLinger`, then the window empties itself.
    private func lingerEndedCall() {
        let ended = call
        Task {
            try? await Task.sleep(for: Self.endedLinger)
            if call == ended { dismissEndedCall() }
        }
    }

    func toggleCallMuted() {
        guard var current = call, current.phase != .ended else { return }
        guard let client else { return showError("Hush isn't running.") }
        current.muted.toggle()
        do {
            try client.setCallMuted(muted: current.muted)
            call = current
        } catch { showError("Couldn't change the microphone: \(Self.describe(error))") }
    }

    /// The ended state was shown long enough: close the call window.
    func dismissEndedCall() {
        if call?.phase == .ended { call = nil }
    }

    /// Brings the call window back (closed mid-call, or hidden behind others).
    func showCallWindow() {
        WindowOpener.open?(id: "call")
    }

    func callAudioDevices() async -> AudioDevices? {
        try? await client?.callAudioDevices()
    }

    func setCallAudioInput(_ id: String) {
        do { try client?.setCallAudioInput(id: id) } catch { showError(Self.describe(error)) }
    }

    func setCallAudioOutput(_ id: String) {
        do { try client?.setCallAudioOutput(id: id) } catch { showError(Self.describe(error)) }
    }

    /// "Continue without history": stops the transfer; chats keep working and new messages flow.
    func cancelHistory() {
        guard let client else { return showError("Hush isn't running.") }
        do { try client.cancelHistory() } catch { showError("Couldn't stop the transfer: \(Self.describe(error))") }
    }

    /// After a failed transfer: picks it up from where it stopped.
    func retryHistory() {
        guard let client else { return showError("Hush isn't running.") }
        let before = history
        history = HistoryProgress(stage: .waiting, done: 0, total: 0)
        do { try client.retryHistory() } catch {
            // the overlay must not promise progress that was never asked for
            history = before
            showError("Couldn't retry the transfer: \(Self.describe(error))")
        }
    }

    /// Settings ▸ Transfer Message History…: this Mac relinks as a new device; everything shown
    /// here is kept and the phone's history merges in. Returns why it couldn't start, or nil:
    /// the QR code is on its way.
    func relinkForHistory() async -> String? {
        guard let client else { return "Hush isn't running." }
        do { try await client.relink() } catch {
            Self.logger.error("relink failed: \(Self.describe(error), privacy: .private)")
            return Self.describe(error)
        }
        await restart(keepKey: true)
        return nil
    }

    /// Settings ▸ Back Up…: writes every chat and file here to `url`, encrypted with
    /// `passphrase`. Why it failed, or nil.
    func backUp(to url: URL, passphrase: String) async -> String? {
        guard let client else { return "Hush isn't running." }
        do { try await client.backUp(path: url.path, passphrase: passphrase) } catch {
            append("backing up failed: \(error)")
            return "Couldn't back up: \(Self.fileFailure(error))"
        }
        return nil
    }

    /// The link screen's Restore from a Backup…: the backup at `url` replaces everything here,
    /// and a new linking code follows. Why it failed while the code stays (a wrong passphrase, a
    /// damaged file), or nil: the core stopped and started over, and `restoreNote` says how it
    /// went.
    func restoreBackup(from url: URL, passphrase: String) async -> String? {
        guard let client else { return "Hush isn't running." }
        restoreNote = nil
        let note: String
        do {
            try await client.restoreBackup(path: url.path, passphrase: passphrase)
            note = "Your backup is restored. Link this Mac to your phone to finish."
        } catch CoreError.InvalidInput(let message) {
            return message
        } catch {
            append("restoring a backup failed: \(error)")
            note = "Couldn't restore the backup: \(Self.fileFailure(error))"
        }
        await restart(keepKey: true)
        restoreNote = note
        return nil
    }

    /// Unlinks this Mac and removes all its data, telling the server first if `tellServer`.
    /// Returns why the server couldn't be told (nothing is removed then: the caller can offer to
    /// unlink anyway), or nil: done and a new linking code is on its way, or the core stopped but
    /// its data couldn't all be removed (the failed screen says so).
    func unlinkThisMac(tellServer: Bool) async -> String? {
        guard let client else { return "Hush isn't running." }
        do {
            try await client.unlink(tellServer: tellServer)
        } catch CoreError.Network(let message) {
            Self.logger.error("unlink failed: \(message, privacy: .private)")
            return Self.describe(CoreError.Network(message: message))
        } catch {
            // the core has stopped: there is no going back to the chats
            Self.logger.error("removing the data failed: \(Self.describe(error), privacy: .private)")
            screen = .failed(
                "Hush couldn't remove its data (\(Self.describe(error))). Quit Hush, delete "
                    + "~/Library/Application Support/Hush, and open it again.")
            return nil
        }
        await restart()
        return nil
    }

    /// Starts over with a fresh, unlinked core: everything shown is forgotten. The key goes
    /// with the data it protected, except when relinking (`keepKey`): the data stays, so its
    /// key and the shown notifications stay too.
    private func restart(keepKey: Bool = false) async {
        listenerGeneration += 1
        client = nil
        if !keepKey {
            forgetKey()
            notifier?.removeAll()  // message previews go with the data
        }
        history = nil
        call = nil
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
    /// A search is on (spaces alone aren't one).
    var isSearching: Bool { !searchQuery.trimmingCharacters(in: .whitespaces).isEmpty }

    /// The chats the sidebar lists: the inbox or the archive, or every match while searching.
    var visibleConversations: [Conversation] {
        if isSearching {
            return conversations.filter { Formatting.matches(title($0), query: searchQuery) }
        }
        return showingArchive ? ChatList.archived(conversations) : ChatList.inbox(conversations)
    }

    var archivedCount: Int { ChatList.archived(conversations).count }

    /// The sidebar shows the archived chats instead of the inbox.
    private(set) var showingArchive = false

    func showArchive(_ on: Bool) { showingArchive = on }

    func title(_ c: Conversation) -> String { Self.displayTitle(c, ownAci: ownAci) }

    /// A conversation's name ("Note to Self" for our own), or a short form of its id until the
    /// name is known.
    nonisolated static func displayTitle(_ c: Conversation, ownAci: String? = nil) -> String {
        if let ownAci, c.id == "c:\(ownAci)" { return "Note to Self" }
        if !c.title.isEmpty { return c.title }
        if c.id.hasPrefix("g:") { return "Group" }
        if let username = c.username { return "@\(username)" }
        return String(c.id.dropFirst(2).prefix(8))
    }

    /// A group member's display name: their contact name, else a short id. Never empty.
    func senderName(_ author: String) -> String {
        senderNames[author] ?? String(author.prefix(8))
    }

    /// A mentioned person's name; a mention of us reads "@You".
    func mentionName(_ aci: String) -> String {
        aci == ownAci ? "You" : senderName(aci)
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
            if case .poll = m.kind {
                do {
                    let poll = try await client.poll(key: key)
                    if generation == selectionGeneration { polls[key] = poll }
                } catch { append("query failed: \(error)") }
            }
            if isGroup, !m.isOutgoing { await loadName(m.author) }
            for aci in MessageText.mentioned(m.ranges + (m.quote?.ranges ?? [])) { await loadName(aci) }
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

    /// Reading a group's invite link.
    enum LinkRead: Equatable {
        case group(GroupPreview)
        case failed(String)
    }

    /// What a group's invite link shows.
    func previewGroupLink(_ link: String) async -> LinkRead {
        guard let client else { return .failed("Not linked yet.") }
        do {
            return .group(try await client.groupLinkPreview(link: link))
        } catch {
            // never the link: it holds the group's key
            append("group link preview failed: \(error)")
            return .failed(Self.serverFailure("Couldn't read the link", error))
        }
    }

    /// Joins a group through its link, or asks to, and opens its chat; nil, or why it failed.
    func joinGroup(_ link: String) async -> String? {
        guard let client else { return "Not linked yet." }
        do {
            let joined = try await client.joinGroup(link: link)
            select(joined.conversationId)
            refreshConversations()
            return nil
        } catch {
            append("joining a group failed: \(error)")
            return Self.serverFailure("Couldn't join the group", error)
        }
    }

    /// Withdraws our request to join a group, and deletes its chat; nil, or why it failed (the
    /// chat then stays).
    func cancelJoinRequest(_ conversationId: String) async -> String? {
        guard let client else { return "Not linked yet." }
        do {
            try await client.cancelJoinRequest(conversationId: conversationId)
        } catch {
            append("cancelling a join request failed: \(error)")
            return Self.serverFailure("Couldn't cancel the request", error)
        }
        deleteChat(conversationId)
        return nil
    }

    /// Reading or saving our profile.
    enum ProfileRead: Equatable {
        case profile(OwnProfile)
        case failed(String)
    }

    /// Our name and about as the server has them now.
    func ownProfile() async -> ProfileRead {
        guard let client else { return .failed("Not linked yet.") }
        do {
            return .profile(try await client.ownProfile())
        } catch {
            append("reading our profile failed: \(error)")
            return .failed(Self.serverFailure("Couldn't read your profile", error))
        }
    }

    /// Saves our name and about; the profile as saved (trimmed), or why it failed.
    func saveProfile(givenName: String, familyName: String, about: String) async -> ProfileRead {
        guard let client else { return .failed("Not linked yet.") }
        do {
            return .profile(try await client.setProfile(givenName: givenName, familyName: familyName, about: about))
        } catch {
            append("saving our profile failed: \(error)")
            return .failed(Self.serverFailure("Couldn't save your profile", error))
        }
    }

    /// A clicked link: a group's invite link opens the join sheet (true); others are the
    /// system's.
    func handleLink(_ url: URL) -> Bool {
        guard url.scheme == "https" || url.scheme == "sgnl", url.host() == "signal.group",
            url.fragment?.isEmpty == false
        else { return false }
        joiningLink = url.absoluteString
        return true
    }

    /// `what` failed: the core's reason (the server's refusal is not "couldn't be reached").
    private static func serverFailure(_ what: String, _ error: Error) -> String {
        switch error {
        case CoreError.Network(let message): "\(what): \(message)"
        default: "\(what): \(describe(error))"
        }
    }

    /// Reading a group for its info panel.
    enum GroupRead: Equatable {
        /// Its members' and invitees' names are loaded.
        case known(GroupInfo)
        /// Not stored yet (no message has come in it).
        case unknown
        case failed(String)
    }

    func groupInfo(_ conversationId: String) async -> GroupRead {
        guard let client else { return .failed("Not linked yet.") }
        do {
            let info = try await client.groupInfo(conversationId: conversationId)
            let people = (info?.members.map(\.aci) ?? []) + (info?.invited ?? []) + (info?.requesting ?? [])
            for aci in people where aci != ownAci {
                await loadName(aci)
            }
            return info.map(GroupRead.known) ?? .unknown
        } catch {
            // ids only: a group's id is its key
            append("group info failed: \(error)")
            return .failed("Couldn't read the group: \(Self.describe(error))")
        }
    }

    /// A group member's name; we are "You".
    func memberName(_ aci: String) -> String {
        aci == ownAci ? "You" : senderName(aci)
    }

    /// Changes a group, one change after the other (the core updates the chat as each lands).
    /// Returns why the first that failed did, or nil.
    func changeGroup(
        _ conversationId: String, _ changes: [GroupChange], failing: String = "Couldn't change the group"
    ) async -> String? {
        guard let client else { return "Not linked yet." }
        var failure: String?
        for change in changes {
            do {
                try await client.changeGroup(conversationId: conversationId, change: change)
            } catch {
                append("couldn't change a group: \(error)")
                guard failure == nil else { continue }
                switch error {
                // the server's refusal, or no answer: not only "couldn't be reached"
                case CoreError.Network(let message): failure = "\(failing): \(message)"
                default: failure = "\(failing): \(Self.describe(error))"
                }
            }
        }
        return failure
    }

    /// Makes a group and opens its chat; nil, or why it failed.
    func createGroup(_ title: String, members: [String]) async -> String? {
        guard let client else { return "Not linked yet." }
        do {
            let id = try await client.createGroup(title: title, members: members)
            select(id)
            refreshConversations()
            return nil
        } catch {
            append("couldn't make a group: \(error)")
            switch error {
            case CoreError.Network(let message): return "Couldn't make the group: \(message)"
            default: return "Couldn't make the group: \(Self.describe(error))"
            }
        }
    }

    /// Answers a group's invitation: accepting joins it (and accepts the chat), declining also
    /// deletes its chat. nil, or why it failed (the chat then stays).
    func answerInvitation(_ conversationId: String, accept: Bool) async -> String? {
        let failing = accept ? "Couldn't join the group" : "Couldn't decline the invitation"
        if let failure = await changeGroup(
            conversationId, [accept ? .acceptInvitation : .declineInvitation], failing: failing)
        {
            return failure
        }
        if !accept { deleteChat(conversationId) }
        return nil
    }

    /// People who can be added to a group: those we have a chat with (accepted, not blocked,
    /// known by account), but us and `members`, by name.
    func addableContacts(excluding members: Set<String>) -> [(aci: String, name: String)] {
        conversations.compactMap { c -> (aci: String, name: String)? in
            guard c.kind == .direct, !c.blocked, !c.messageRequest, !c.id.hasPrefix("c:PNI:") else { return nil }
            let aci = String(c.id.dropFirst(2))
            guard aci != ownAci, !members.contains(aci) else { return nil }
            return (aci, title(c))
        }
        .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    /// Who can be mentioned in a group: its members but us, each with a name (ACI, name).
    func mentionCandidates(_ conversationId: String) async -> [(String, String)] {
        guard let client, conversationId.hasPrefix("g:") else { return [] }
        do {
            let members = try await client.groupMembers(conversationId: conversationId).filter { $0 != ownAci }
            for aci in members { await loadName(aci) }
            return members.map { ($0, senderName($0)) }
        } catch {
            append("query failed: \(error)")
            return []
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
                    // a blocked chat's or a request's picture isn't shown (a stranger chose it)
                    AvatarPictures.shared.paths = Dictionary(
                        found.filter { !$0.blocked && !$0.messageRequest }.compactMap { c in c.avatar.map { (c.id, $0) }
                        },
                        uniquingKeysWith: { a, _ in a })
                    Self.logger.notice("conversations: \(self.conversations.count, privacy: .public)")
                    // group rows say who wrote the last message
                    for c in conversations where c.kind == .group && !c.lastMessageOutgoing {
                        await loadName(c.lastMessageAuthor)
                    }
                    for c in conversations {
                        for aci in MessageText.mentioned(c.lastMessageRanges) { await loadName(aci) }
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

    /// A backup's file error as it is ("disk full"), not as a failure to open Hush's data.
    private static func fileFailure(_ error: Error) -> String {
        if case CoreError.Storage(let message) = error { message } else { describe(error) }
    }

    private static func describe(_ error: Error) -> String {
        switch error {
        case CoreError.AlreadyOpen:
            return "Hush is already running."
        case Legacy.MoveError.stillOpen:
            return "Quit \(Legacy.name) and reopen Hush: it moves \(Legacy.name)'s data over once nothing has it open."
        case KeychainError.status(errSecInteractionNotAllowed), KeychainError.status(errSecAuthFailed),
            KeychainError.status(errSecUserCanceled):
            return "Hush couldn't read its key from your keychain. Unlock the keychain, allow access, and reopen."
        case CoreError.Storage(let message):
            return "Hush couldn't open its data: \(message)"
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

/// The link a preview is for: the first https link, as typed (as the core finds it).
enum PreviewLink {
    static func first(in text: String) -> String? {
        for word in text.split(whereSeparator: \.isWhitespace) {
            guard let start = word.range(of: "https://") else { continue }
            let link = String(word[start.lowerBound...]).trimmingCharacters(
                in: CharacterSet(charactersIn: ".,;:!?)]>\"'"))
            if let url = URL(string: link), url.host() != nil { return link }
        }
        return nil
    }
}
