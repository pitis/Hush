import Foundation

/// Search snippets from the core mark matches with U+E000 … U+E001.
enum SearchText {
    static let start: Character = "\u{E000}"
    static let end: Character = "\u{E001}"

    /// The snippet with its matches bold.
    static func highlighted(_ snippet: String) -> AttributedString {
        var out = AttributedString()
        var inMatch = false
        var run = ""
        func flush() {
            var part = AttributedString(run)
            if inMatch { part.inlinePresentationIntent = .stronglyEmphasized }
            out += part
            run = ""
        }
        for ch in snippet {
            if ch == start || ch == end {
                flush()
                inMatch = ch == start
            } else {
                run.append(ch)
            }
        }
        flush()
        return out
    }
}
