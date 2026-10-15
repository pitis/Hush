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

    static func line(_ c: Conversation, selected: Bool, typing: Bool, draft: String, authorName: String) -> Line {
        if typing { return .typing }
        if !selected, Composer.canSend(draft) { return .draft(oneLine(draft)) }
        guard c.lastMessageAt > 0 else { return .message("", tick: nil) }
        // the core's notice; the older message's author, ticks and files don't belong to it
        if c.lastMessageUndecryptable { return .message(c.lastMessagePreview, tick: nil) }
        let author = c.lastMessageOutgoing ? "You" : authorName
        if c.lastMessageDeleted { return .message("\(author) deleted this message", tick: nil) }
        let text =
            if let file = c.lastMessageFile {
                oneLine(Files.describe(file, body: c.lastMessagePreview))
            } else {
                c.lastMessagePreview.isEmpty ? "Attachment" : oneLine(c.lastMessagePreview)
            }
        let tick = c.lastMessageOutgoing ? Timeline.tick(for: c.lastMessageStatus) : nil
        return .message(c.kind == .group ? "\(author): \(text)" : text, tick: tick)
    }

    /// The chat `step` away from `selected` (−1 up, +1 down), as Desktop's ⌥↑/⌥↓: wraps around
    /// the list; with `unreadOnly` (⌥⇧↑/⌥⇧↓) the nearest unread chat that way, without wrapping.
    static func neighbor(in list: [Conversation], of selected: String?, step: Int, unreadOnly: Bool) -> String? {
        let up = step < 0
        let unread = { (c: Conversation) in c.unreadCount > 0 }
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

    /// The dock badge: unread messages in all chats (Desktop's default), nil when none.
    static func badge(_ list: [Conversation]) -> String? {
        let total = list.reduce(0) { $0 + Int($1.unreadCount) }
        return total > 0 ? "\(total)" : nil
    }

    /// The open chat is read once its newest message is on screen in the key window, and the
    /// window is actually visible (not while the screen is locked or asleep).
    static func shouldMarkRead(atBottom: Bool, windowKey: Bool, windowVisible: Bool, unread: UInt32) -> Bool {
        atBottom && windowKey && windowVisible && unread > 0
    }

    private static func oneLine(_ text: String) -> String {
        text.split(whereSeparator: \.isNewline).joined(separator: " ")
    }
}
