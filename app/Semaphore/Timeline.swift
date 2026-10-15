import Foundation

/// The rows of a conversation: day separators and messages, with grouping of runs from the same
/// sender (tight spacing, name and avatar once).
enum Timeline {
    enum Item: Identifiable {
        case day(String, ms: Int64)
        /// `first`/`last`: the message starts/ends a run of the same sender.
        case message(Message, first: Bool, last: Bool)

        var id: String {
            switch self {
            case .day(_, let ms): "day-\(ms)"
            case .message(let m, _, _): "\(m.author)-\(m.sentAt)"
            }
        }
    }

    enum Tick: Equatable { case clock, single, double, doubleFilled, failed }

    static let runGap: Int64 = 3 * 60 * 1000

    static func items(for messages: [Message], calendar: Calendar = .autoupdatingCurrent, now: Date = .now)
        -> [Item]
    {
        let fmt = Formatting(calendar: calendar)
        var items: [Item] = []
        for (i, m) in messages.enumerated() {
            let prev = i > 0 ? messages[i - 1] : nil
            let next = i + 1 < messages.count ? messages[i + 1] : nil
            let newDay = prev.map { !sameDay($0, m, calendar) } ?? true
            if newDay { items.append(.day(fmt.dayTitle(m.sentAt, now: now), ms: m.sentAt)) }
            let first = newDay || !sameRun(prev, m, calendar)
            let last = next.map { !sameRun(m, $0, calendar) } ?? true
            items.append(.message(m, first: first, last: last))
        }
        return items
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
        cal.isDate(date(a), inSameDayAs: date(b))
    }

    private static func sameRun(_ a: Message?, _ b: Message, _ cal: Calendar) -> Bool {
        guard let a else { return false }
        return a.author == b.author && a.isOutgoing == b.isOutgoing && b.sentAt - a.sentAt < runGap
            && sameDay(a, b, cal)
    }

    private static func date(_ m: Message) -> Date { Date(timeIntervalSince1970: Double(m.sentAt) / 1000) }
}
