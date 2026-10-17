import Foundation

/// Text shown in lists and the timeline: times, day titles, previews, initials.
struct Formatting {
    var calendar: Calendar = .autoupdatingCurrent

    /// Sidebar time: "Now", "11m", "09:05" (today), "Tue" (this week), "Jul 18" (this year),
    /// else a short date. Empty for 0.
    func listTime(_ ms: Int64, now: Date = .now) -> String {
        guard ms > 0 else { return "" }
        let date = Date(timeIntervalSince1970: Double(ms) / 1000)
        let seconds = now.timeIntervalSince(date)
        if seconds < 60 { return "Now" }
        if seconds < 3600 { return "\(Int(seconds / 60))m" }
        if calendar.isDate(date, inSameDayAs: now) { return format(date, "HH:mm") }
        if let days = calendar.dateComponents([.day], from: calendar.startOfDay(for: date), to: now).day, days < 7 {
            return format(date, "EEE")
        }
        if calendar.component(.year, from: date) == calendar.component(.year, from: now) {
            return format(date, "MMM d")
        }
        return format(date, "M/d/yy")
    }

    /// Bubble time in the user's 12/24-hour style ("09:05" or "9:05 AM").
    func bubbleTime(_ ms: Int64) -> String {
        Formatting.cached(key: "time-\(calendar.timeZone.identifier)") {
            let f = DateFormatter()
            f.calendar = calendar
            f.timeZone = calendar.timeZone
            f.timeStyle = .short
            f.dateStyle = .none
            return f
        }.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }

    /// Timeline separator: "Today", "Yesterday", "Mon, Oct 12", or with the year if not this year.
    func dayTitle(_ ms: Int64, now: Date = .now) -> String {
        let date = Date(timeIntervalSince1970: Double(ms) / 1000)
        if calendar.isDate(date, inSameDayAs: now) { return "Today" }
        if let yesterday = calendar.date(byAdding: .day, value: -1, to: now),
            calendar.isDate(date, inSameDayAs: yesterday)
        {
            return "Yesterday"
        }
        if calendar.component(.year, from: date) == calendar.component(.year, from: now) {
            return format(date, "EEE, MMM d")
        }
        return format(date, "MMM d, yyyy")
    }

    private func format(_ date: Date, _ pattern: String) -> String {
        Formatting.cached(key: "\(pattern)-\(calendar.timeZone.identifier)") {
            let f = DateFormatter()
            f.calendar = calendar
            f.timeZone = calendar.timeZone
            f.locale = Locale(identifier: "en_US_POSIX")
            f.dateFormat = pattern
            return f
        }.string(from: date)
    }

    /// Formatters are expensive to create; the UI asks for the same few over and over.
    nonisolated(unsafe) private static var formatters: [String: DateFormatter] = [:]  // main thread only
    private static func cached(key: String, make: () -> DateFormatter) -> DateFormatter {
        if let f = formatters[key] { return f }
        let f = make()
        formatters[key] = f
        return f
    }

    /// Up to two initials from the first letters/digits of the first two words; "#" if none.
    static func initials(_ name: String) -> String {
        // letters and digits only; emoji like 🅰️ count as letters to Unicode but aren't initials
        let letters = name.split(separator: " ").compactMap { word in
            word.first(where: { ch in
                guard let scalar = ch.unicodeScalars.first, ch.unicodeScalars.count == 1 else { return false }
                return (ch.isLetter && !scalar.properties.isEmoji) || ch.isNumber
            })
        }
        let result = String(letters.prefix(2)).uppercased()
        return result.isEmpty ? "#" : result
    }

    static let avatarPaletteSize = 8

    /// A stable palette index for an id (same id → same color across launches).
    static func avatarIndex(_ id: String) -> Int {
        var hash: UInt32 = 2_166_136_261
        for byte in id.utf8 { hash = (hash ^ UInt32(byte)) &* 16_777_619 }
        return Int(hash % UInt32(avatarPaletteSize))
    }

    /// Case-insensitive subsequence match ("hbh" matches "He Be Hatin"); blank matches all.
    static func matches(_ text: String, query: String) -> Bool {
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return true }
        let t = text.lowercased()
        if t.contains(q) { return true }
        var i = t.startIndex
        for ch in q where ch != " " {
            guard let found = t[i...].firstIndex(of: ch) else { return false }
            i = t.index(after: found)
        }
        return true
    }
}
