import Foundation

/// The rows of a conversation: day separators and messages, with grouping of runs from the same
/// sender (tight spacing, name and avatar once).
enum Timeline {
    enum Item: Identifiable {
        case day(String, ms: Int64)
        /// `first`/`last`: the message starts/ends a run of the same sender.
        case message(Message, first: Bool, last: Bool)
        case event(ChatEvent)
        /// Messages that couldn't be decrypted, one after another: one notice, from the first.
        /// `reset`: the secure session was reset after them.
        case failures(ChatEvent, count: Int, reset: Bool)

        var id: String {
            switch self {
            case .day(_, let ms): "day-\(ms)"
            case .message(let m, _, _): "\(m.author)-\(m.sentAt)"
            case .event(let e), .failures(let e, _, _): "event-\(e.id)"
            }
        }
    }

    enum Tick: Equatable { case clock, single, double, doubleFilled, failed }

    static let runGap: Int64 = 3 * 60 * 1000

    /// Day separators, messages (grouped in runs) and chat events, in time order. An event
    /// breaks a run of messages.
    static func items(
        for messages: [Message], events: [ChatEvent] = [], calendar: Calendar = .autoupdatingCurrent,
        now: Date = .now
    ) -> [Item] {
        let fmt = Formatting(calendar: calendar)
        let entries: [Entry] = (messages.map(Entry.message) + events.map(Entry.event)).enumerated()
            .sorted { ($0.element.at, $0.offset) < ($1.element.at, $1.offset) }.map(\.element)
        var items: [Item] = []
        for (i, entry) in entries.enumerated() {
            let prev = i > 0 ? entries[i - 1] : nil
            let newDay = prev.map { !calendar.isDate(date($0.at), inSameDayAs: date(entry.at)) } ?? true
            if newDay { items.append(.day(fmt.dayTitle(entry.at, now: now), ms: entry.at)) }
            switch entry {
            case .event(let e) where e.kind == .decryptionFailed:
                if !newDay, case .failures(let first, let count, _) = items.last {
                    items[items.count - 1] = .failures(first, count: count + 1, reset: false)
                } else {
                    items.append(.failures(e, count: 1, reset: false))
                }
            case .event(let e):
                items.append(.event(e))
            case .message(let m):
                let next = i + 1 < entries.count ? entries[i + 1].message : nil
                let first = newDay || !sameRun(prev?.message, m, calendar)
                let last = next.map { !sameRun(m, $0, calendar) } ?? true
                items.append(.message(m, first: first, last: last))
            }
        }
        // a reset answers the failures before it
        var resetLater = false
        for i in items.indices.reversed() {
            switch items[i] {
            case .event(let e) where e.kind == .sessionReset: resetLater = true
            case .failures(let e, let count, _): items[i] = .failures(e, count: count, reset: resetLater)
            default: break
            }
        }
        return items
    }

    private enum Entry {
        case message(Message)
        case event(ChatEvent)

        var at: Int64 {
            switch self {
            case .message(let m): m.sentAt
            case .event(let e): e.at
            }
        }

        var message: Message? {
            if case .message(let m) = self { return m }
            return nil
        }
    }

    /// What the timeline does when its last message changes: follow it when the reader is at the
    /// bottom or it is their own message, otherwise show the "New messages" pill.
    enum NewMessageAction: Equatable { case scrollToBottom, showPill, nothing }

    static func onNewLastMessage(atBottom: Bool, isOwn: Bool) -> NewMessageAction {
        atBottom || isOwn ? .scrollToBottom : .showPill
    }

    /// Within this many points of the edge counts as being there.
    static let edgeThreshold: CGFloat = 80

    /// `hiddenBelow`: how much of the visible height something floats over (the composer).
    static func isAtBottom(
        offsetY: CGFloat, visibleHeight: CGFloat, contentHeight: CGFloat, hiddenBelow: CGFloat = 0
    ) -> Bool {
        offsetY + visibleHeight - hiddenBelow >= contentHeight - edgeThreshold
    }

    static func isNearTop(offsetY: CGFloat) -> Bool { offsetY <= edgeThreshold }

    static func accessibilityLabel(for tick: Tick) -> String {
        switch tick {
        case .clock: "Sending"
        case .single: "Sent"
        case .double: "Delivered"
        case .doubleFilled: "Read"
        case .failed: "Not sent"
        }
    }

    static func tick(for status: MessageStatus) -> Tick {
        switch status {
        case .pending: .clock
        case .sent: .single
        case .delivered: .double
        case .read: .doubleFilled
        case .failed: .failed
        }
    }

    private static func sameDay(_ a: Message, _ b: Message, _ cal: Calendar) -> Bool {
        cal.isDate(date(a.sentAt), inSameDayAs: date(b.sentAt))
    }

    private static func sameRun(_ a: Message?, _ b: Message, _ cal: Calendar) -> Bool {
        guard let a else { return false }
        return a.author == b.author && a.isOutgoing == b.isOutgoing && b.sentAt - a.sentAt < runGap
            && sameDay(a, b, cal)
    }

    private static func date(_ ms: Int64) -> Date { Date(timeIntervalSince1970: Double(ms) / 1000) }
}
