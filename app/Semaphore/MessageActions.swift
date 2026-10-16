import Foundation

/// What can be done to a message, after Signal Desktop: react, edit and delete for everyone.
enum MessageActions {
    /// Signal's default reactions.
    static let quickReactions = ["❤️", "👍", "👎", "😂", "😮", "😢"]
    /// Signal lets you edit or delete a message for everyone this long after sending it.
    static let changeWindow: TimeInterval = 24 * 60 * 60

    /// Our own message, sent, not deleted, within the window (the core checks this again).
    static func canDelete(_ m: Message, me: String?, now: Date = .now) -> Bool {
        guard let me, m.author == me, m.isOutgoing, !m.deleted, [.sent, .delivered, .read].contains(m.status) else {
            return false
        }
        return now.timeIntervalSince1970 * 1000 - Double(m.sentAt) <= changeWindow * 1000
    }

    /// As `canDelete`, and it has text to edit, without mentions: the composer can't write those
    /// yet, so an edit would strip them.
    static func canEdit(_ m: Message, me: String?, now: Date = .now) -> Bool {
        canDelete(m, me: me, now: now) && !(m.body ?? "").isEmpty
            && !m.ranges.contains {
                if case .mention = $0.style { return true }
                return false
            }
    }

    /// A picked character that can be a reaction: an emoji, not a digit or symbol that only has
    /// an emoji form ("1", "#", "©").
    static func isReaction(_ s: String) -> Bool {
        guard s.count == 1, let first = s.unicodeScalars.first else { return false }
        return s.unicodeScalars.count > 1 || first.properties.isEmojiPresentation
    }

    /// Our reaction among a message's reactions.
    static func mine(_ reactions: [Reaction], me: String?) -> String? {
        reactions.first { $0.reactor == me }?.emoji
    }

    /// What to send when `emoji` is chosen: our current reaction again takes it back, any other
    /// replaces it (one reaction each).
    static func reaction(choosing emoji: String, mine: String?) -> (emoji: String, remove: Bool) {
        emoji == mine ? (emoji, true) : (emoji, false)
    }

    struct PillEntry: Equatable {
        let emoji: String
        let count: Int
        let mine: Bool
    }

    /// The reaction pill: each emoji once with how many, most first, ours marked.
    static func pill(_ reactions: [Reaction], me: String?) -> [PillEntry] {
        var order: [String] = []
        var counts: [String: Int] = [:]
        for r in reactions {
            if counts[r.emoji] == nil { order.append(r.emoji) }
            counts[r.emoji, default: 0] += 1
        }
        return order.map { PillEntry(emoji: $0, count: counts[$0] ?? 0, mine: $0 == mine(reactions, me: me)) }
            .enumerated().sorted { ($1.element.count, $0.offset) < ($0.element.count, $1.offset) }.map(\.element)
    }

    /// ↑ in an empty composer: the newest message of ours that can still be edited.
    static func lastEditable(_ messages: [Message], me: String?, now: Date = .now) -> Message? {
        messages.last { canEdit($0, me: me, now: now) }
    }
}
