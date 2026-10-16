import SwiftUI

/// Signal's text styles and mentions (the core's `BodyRange`s), shown as Desktop shows them.
/// Ranges count UTF-16 code units; one that doesn't fit the text is ignored.
enum MessageText {
    /// A run's styles, kept on the text; `text(_:size:)` draws them.
    struct Styles: OptionSet, Hashable {
        let rawValue: Int
        static let bold = Styles(rawValue: 1)
        static let italic = Styles(rawValue: 2)
        static let strikethrough = Styles(rawValue: 4)
        static let monospace = Styles(rawValue: 8)
    }

    enum StylesKey: AttributedStringKey {
        typealias Value = Styles
        static let name = "semaphore.styles"
    }

    /// What a hidden spoiler reads as where the text is one line (chat list, notifications).
    static let hiddenSpoiler = "■■■■"

    /// `body` styled for a bubble: mentions read "@name", spoilers are covered until revealed.
    /// Each run's styles are under `StylesKey`; `text(_:size:)` draws them.
    static func attributed(
        _ body: String, ranges: [BodyRange], name: (String) -> String, revealSpoilers: Bool = false
    ) -> AttributedString {
        var text = AttributedString(body)
        for r in ranges {
            guard let span = span(r, in: text) else { continue }
            switch r.style {
            case .bold: add(.bold, to: span, in: &text)
            case .italic: add(.italic, to: span, in: &text)
            case .strikethrough: add(.strikethrough, to: span, in: &text)
            case .monospace: add(.monospace, to: span, in: &text)
            case .spoiler where !revealSpoilers:
                text[span].foregroundColor = .clear
                text[span].backgroundColor = .white.opacity(0.45)
            case .spoiler, .mention: break
            }
        }
        // last first, so the offsets of the earlier ones still hold
        for r in mentions(ranges) {
            guard case .mention(let aci) = r.style, let span = span(r, in: text) else { continue }
            var mention = AttributedString("@\(name(aci))")
            mention[StylesKey.self] = .bold
            text.replaceSubrange(span, with: mention)
        }
        let struck = text.runs.compactMap { run in
            run[StylesKey.self]?.contains(.strikethrough) == true ? run.range : nil
        }
        for range in struck { text[range].strikethroughStyle = .single }
        return text
    }

    /// `body` as plain text: mentions as "@name", spoilers hidden unless `revealSpoilers` (Copy).
    static func plain(
        _ body: String, ranges: [BodyRange], name: (String) -> String, revealSpoilers: Bool = false
    ) -> String {
        let text = NSMutableString(string: body)
        var end = text.length
        // last first; a range overlapping one already replaced is skipped
        let replaced = ranges.filter {
            if case .mention = $0.style { return true }
            return $0.style == .spoiler && !revealSpoilers
        }
        for r in replaced.sorted(by: { $0.start > $1.start }) {
            let start = Int(r.start)
            let length = Int(r.length)
            guard start + length <= end, length > 0 else { continue }
            let with =
                switch r.style {
                case .mention(let aci): "@\(name(aci))"
                default: hiddenSpoiler
                }
            text.replaceCharacters(in: NSRange(location: start, length: length), with: with)
            end = start
        }
        return text as String
    }

    static func hasSpoiler(_ ranges: [BodyRange]) -> Bool {
        ranges.contains { $0.style == .spoiler }
    }

    /// The people mentioned, once each, so their names can be looked up.
    static func mentioned(_ ranges: [BodyRange]) -> [String] {
        var seen = Set<String>()
        return ranges.compactMap {
            guard case .mention(let aci) = $0.style, seen.insert(aci).inserted else { return nil }
            return aci
        }
    }

    private static func mentions(_ ranges: [BodyRange]) -> [BodyRange] {
        ranges.filter {
            if case .mention = $0.style { return true }
            return false
        }
        .sorted { $0.start > $1.start }
    }

    private static func span(_ r: BodyRange, in text: AttributedString) -> Range<AttributedString.Index>? {
        guard r.length > 0 else { return nil }
        return Range(NSRange(location: Int(r.start), length: Int(r.length)), in: text)
    }

    /// `attributed` as a `Text` at `size` points: one piece per run, each in its font.
    static func text(_ attributed: AttributedString, size: CGFloat) -> Text {
        attributed.runs.reduce(Text(verbatim: "")) { text, run in
            let styles = run[StylesKey.self] ?? []
            var piece = Text(AttributedString(attributed[run.range]))
                .font(.system(size: size, design: styles.contains(.monospace) ? .monospaced : .default))
            if styles.contains(.bold) { piece = piece.bold() }
            if styles.contains(.italic) { piece = piece.italic() }
            return text + piece
        }
    }

    /// Adds `style` to each run in `span`, keeping the styles already there (ranges overlap).
    private static func add(_ style: Styles, to span: Range<AttributedString.Index>, in text: inout AttributedString) {
        let runs = text[span].runs.map { ($0.range, $0[StylesKey.self] ?? []) }
        for (range, current) in runs { text[range][StylesKey.self] = current.union(style) }
    }
}
