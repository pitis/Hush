import Foundation

/// The chat list's rules, after Signal Desktop: what a row says, moving between chats from the
/// keyboard, the dock badge, and when the open chat counts as read.
enum ChatList {
    /// A row's second line.
    enum Line: Equatable {
        case typing
        /// Unsent text of a chat that isn't open ("Draft: …").
        case draft(String)
        /// The last message as Desktop words it, with ticks for our own.
        case message(String, tick: Timeline.Tick?)
    }

    /// `name` gives a mentioned person's name.
    static func line(
        _ c: Conversation, selected: Bool, typing: Bool, draft: String, authorName: String,
        name: (String) -> String = { String($0.prefix(8)) }
    ) -> Line {
        if typing { return .typing }
        // as Desktop: what the chat is, not what was said in it
        if c.blocked { return .message("Blocked", tick: nil) }
        if c.groupInvitation { return .message("Group invitation", tick: nil) }
        if c.messageRequest { return .message("Message request", tick: nil) }
        if !selected, Composer.canSend(draft) { return .draft(oneLine(draft)) }
        guard c.lastMessageAt > 0 else { return .message("", tick: nil) }
        // the core's notice; the older message's author, ticks and files don't belong to it
        if c.lastMessageUndecryptable { return .message(c.lastMessagePreview, tick: nil) }
        let author = c.lastMessageOutgoing ? "You" : authorName
        let preview = MessageText.plain(c.lastMessagePreview, ranges: c.lastMessageRanges, name: name)
        if c.lastMessageDeleted { return .message("\(author) deleted this message", tick: nil) }
        let text =
            if let placeholder = Placeholder.text(c.lastMessageKind) {
                placeholder
            } else if let file = c.lastMessageFile {
                oneLine(Files.describe(file, body: preview))
            } else {
                preview.isEmpty ? "Attachment" : oneLine(preview)
            }
        let tick = c.lastMessageOutgoing ? Timeline.tick(for: c.lastMessageStatus) : nil
        return .message(c.kind == .group ? "\(author): \(text)" : text, tick: tick)
    }

    /// The chat `step` away from `selected` (−1 up, +1 down), as Desktop's ⌥↑/⌥↓: wraps around
    /// the list; with `unreadOnly` (⌥⇧↑/⌥⇧↓) the nearest unread chat that way, without wrapping.
    static func neighbor(in list: [Conversation], of selected: String?, step: Int, unreadOnly: Bool) -> String? {
        let up = step < 0
        let unread = isUnread
        guard let i = selected.flatMap({ id in list.firstIndex { $0.id == id } }) else {
            if unreadOnly { return (up ? list.last(where: unread) : list.first(where: unread))?.id }
            return (up ? list.last : list.first)?.id
        }
        if unreadOnly {
            return (up ? list[..<i].last(where: unread) : list[(i + 1)...].first(where: unread))?.id
        }
        let j = i + (up ? -1 : 1)
        if j < 0 { return list.last?.id }
        if j >= list.count { return list.first?.id }
        return list[j].id
    }

    /// The chat at 1-based position `n` (⌘1–⌘9).
    static func at(_ n: Int, in list: [Conversation]) -> String? {
        n >= 1 && n <= list.count ? list[n - 1].id : nil
    }

    /// The dock badge, as Desktop counts it: unread messages in the chats that aren't archived or
    /// muted, plus one per chat marked unread; nil when none.
    static func badge(_ list: [Conversation], now: Date) -> String? {
        let total = list.filter { !$0.archived && !isMuted($0, now: now) }
            .reduce(0) { $0 + ($1.unreadCount > 0 ? Int($1.unreadCount) : ($1.markedUnread ? 1 : 0)) }
        return total > 0 ? "\(total)" : nil
    }

    /// The chats with unread messages, in list order (newest first), for the menu bar: the ones
    /// the badge counts.
    static func unreadChats(_ list: [Conversation], now: Date, limit: Int) -> [Conversation] {
        Array(list.filter { !$0.archived && !isMuted($0, now: now) && isUnread($0) }.prefix(limit))
    }

    /// A menu bar entry: the name, with the unread count when there is one.
    static func menuTitle(_ title: String, _ c: Conversation) -> String {
        c.unreadCount > 0 ? "\(title) (\(c.unreadCount))" : title
    }

    /// How many chats can be pinned (the phone's limit; the core refuses more).
    static let maxPinned = 4

    /// The open chat is read once its newest message is on screen in the key window, and the
    /// window is actually visible (not while the screen is locked or asleep).
    static func shouldMarkRead(atBottom: Bool, windowKey: Bool, windowVisible: Bool, unread: Bool) -> Bool {
        atBottom && windowKey && windowVisible && unread
    }

    /// Unread messages, or marked unread by hand.
    static func isUnread(_ c: Conversation) -> Bool {
        c.unreadCount > 0 || c.markedUnread
    }

    /// Muted at `now` (`muteUntil` is ms since the epoch; `Int64.max` means for good).
    static func isMuted(_ c: Conversation, now: Date) -> Bool {
        c.muteUntil > Int64(now.timeIntervalSince1970 * 1000)
    }

    /// The main list: pinned chats first (the core orders them), without the archived ones.
    static func inbox(_ list: [Conversation]) -> [Conversation] {
        list.filter { !$0.archived }
    }

    static func archived(_ list: [Conversation]) -> [Conversation] {
        list.filter(\.archived)
    }

    /// How long a chat can be muted for, as Desktop offers it.
    enum MuteOption: CaseIterable {
        case hour, eightHours, day, week, always

        var title: String {
            switch self {
            case .hour: "1 hour"
            case .eightHours: "8 hours"
            case .day: "1 day"
            case .week: "1 week"
            case .always: "Always"
            }
        }

        /// When the mute ends, in ms since the epoch (`Int64.max`: never).
        func until(now: Date) -> Int64 {
            let hours: Int64? =
                switch self {
                case .hour: 1
                case .eightHours: 8
                case .day: 24
                case .week: 7 * 24
                case .always: nil
                }
            guard let hours else { return Int64.max }
            return Int64(now.timeIntervalSince1970 * 1000) + hours * 3_600_000
        }
    }

    private static func oneLine(_ text: String) -> String {
        text.split(whereSeparator: \.isNewline).joined(separator: " ")
    }
}
