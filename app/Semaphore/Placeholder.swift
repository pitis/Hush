import Foundation

/// How messages we can't show yet, and disappearing-message timers, are worded (Signal Desktop's
/// words).
enum Placeholder {
    /// "Outgoing voice call (2:05)" — the duration only when it connected.
    static func call(_ what: String, seconds: Int64?) -> String {
        guard let seconds, seconds > 0 else { return what }
        return String(format: "%@ (%d:%02d)", what, seconds / 60, seconds % 60)
    }

    /// A placeholder message's text, or nil for a normal one.
    static func text(_ kind: MessageKind) -> String? {
        switch kind {
        case .normal: nil
        case .sticker(let emoji): [emoji, "Sticker message"].compactMap { $0 }.joined(separator: " ")
        case .contact(let name): "👤 \(name ?? "Contact")"
        case .payment: "💳 Payment"
        case .poll(let question): "📊 Poll: \(question ?? "")"
        case .giftBadge: "✨ Gift badge"
        case .viewOnce: "View-once media"
        }
    }

    /// The timers offered, as in Desktop (seconds; 0: off).
    static let timerChoices: [UInt32] = [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200]

    /// The choices, with `current` among them when the phone set another (2 days, say).
    static func timerChoices(current: UInt32) -> [UInt32] {
        timerChoices.contains(current) ? timerChoices : (timerChoices + [current]).sorted()
    }

    static func timerChoice(_ seconds: UInt32) -> String {
        seconds == 0 ? "Off" : duration(Int64(seconds))
    }

    /// A timer length in its largest whole unit: "30 seconds", "8 hours", "1 week".
    static func duration(_ seconds: Int64) -> String {
        let units: [(Int64, String)] = [(604_800, "week"), (86_400, "day"), (3_600, "hour"), (60, "minute")]
        for (size, name) in units where seconds >= size && seconds % size == 0 {
            let n = seconds / size
            return "\(n) \(name)\(n == 1 ? "" : "s")"
        }
        return "\(seconds) second\(seconds == 1 ? "" : "s")"
    }

    /// A timer change in a chat; `who` is nil when it was us.
    static func timerChange(_ e: ChatEvent, who: String?) -> String {
        let seconds = e.value ?? 0
        let subject = who ?? "You"
        return seconds == 0
            ? "\(subject) disabled disappearing messages."
            : "\(subject) set the disappearing message time to \(duration(seconds))."
    }
}

/// View-once media: one look, then gone (as in Desktop).
enum ViewOnce: Equatable {
    /// Ours: we can't open it.
    case sent
    case viewed
    case download(Int64)
    case downloading
    case open(Attachment)

    static func state(_ m: Message, files: [Attachment]) -> ViewOnce {
        if m.isOutgoing { return .sent }
        guard case .viewOnce(viewed: false) = m.kind, let file = files.first else { return .viewed }
        switch file.state {
        case .ready: return .open(file)
        case .downloading: return .downloading
        case .pending, .failed: return .download(file.id)
        }
    }

    static func label(_ state: ViewOnce) -> String {
        switch state {
        case .sent: "View-once media"
        case .viewed: "Viewed"
        case .download: "Download view-once media"
        case .downloading: "Downloading…"
        case .open(let file): file.contentType.hasPrefix("video/") ? "View video" : "View photo"
        }
    }
}
