import SwiftUI

/// Signal's text styles and mentions (the core's `BodyRange`s), shown as Desktop shows them.
/// Ranges count UTF-16 code units; one that doesn't fit the text is ignored, except a spoiler,
/// which hides up to the end of the text.
enum MessageText {
    /// A run's styles, kept on the text; `text(_:size:)` draws them.
    struct Styles: OptionSet, Hashable {
        let rawValue: Int
        static let bold = Styles(rawValue: 1)
        static let italic = Styles(rawValue: 2)
        static let strikethrough = Styles(rawValue: 4)
        static let monospace = Styles(rawValue: 8)
        /// Only in the composer: bubbles cover spoilers instead.
        static let spoiler = Styles(rawValue: 16)
    }

    enum StylesKey: AttributedStringKey {
        typealias Value = Styles
        static let name = "hush.styles"
    }

    /// What a hidden spoiler reads as where the text is one line (chat list, notifications).
    static let hiddenSpoiler = "■■■■"

    /// `body` styled for a bubble: mentions read "@name", spoilers are covered until revealed.
    /// Each run's styles are under `StylesKey`; `text(_:size:)` draws them.
    static func attributed(
        _ body: String, ranges: [BodyRange], name: (String) -> String, revealSpoilers: Bool = false
    ) -> AttributedString {
        var text = AttributedString(body)
        let length = (body as NSString).length
        for r in ranges {
            guard let span = span(r.style == .spoiler ? clamped(r, to: length) : r, in: text) else { continue }
            switch r.style {
            case .bold: add(.bold, to: span, in: &text)
            case .italic: add(.italic, to: span, in: &text)
            case .strikethrough: add(.strikethrough, to: span, in: &text)
            case .monospace: add(.monospace, to: span, in: &text)
            case .spoiler where !revealSpoilers:
                text[span].foregroundColor = .clear
                // the appearance's text colour, half covering: a grey block on either bubble
                text[span].backgroundColor = .primary.opacity(0.45)
            case .spoiler, .mention: break
            }
        }
        // last first, so the offsets of the earlier ones still hold
        for r in mentions(ranges) {
            guard case .mention(let aci) = r.style, let span = span(r, in: text) else { continue }
            // it takes the placeholder's look: the styles and a spoiler's cover around it
            let around = text[span].runs.first?.attributes ?? AttributeContainer()
            var mention = AttributedString("@\(name(aci))", attributes: around)
            mention[StylesKey.self] = (around[StylesKey.self] ?? []).union(.bold)
            text.replaceSubrange(span, with: mention)
        }
        let struck = text.runs.compactMap { run in
            run[StylesKey.self]?.contains(.strikethrough) == true ? run.range : nil
        }
        for range in struck { text[range].strikethroughStyle = .single }
        // a group's invite link joins here (`AppStore.handleLink`); other links stay text
        for range in groupLinks(in: text) { text[range].link = URL(string: String(text[range].characters)) }
        return text
    }

    /// Where `text` has a group's invite link, but under a hidden spoiler.
    static func groupLinks(in text: AttributedString) -> [Range<AttributedString.Index>] {
        let plain = String(text.characters)
        let link = ##/(?:https|sgnl)://signal\.group/#[A-Za-z0-9_=%-]+/##
        return plain.matches(of: link).compactMap { match in
            let lower = text.characters.index(
                text.startIndex, offsetBy: plain.distance(from: plain.startIndex, to: match.range.lowerBound))
            let upper = text.characters.index(
                lower, offsetBy: plain.distance(from: match.range.lowerBound, to: match.range.upperBound))
            let range = lower..<upper
            return text[range].runs.allSatisfy { $0.backgroundColor == nil } ? range : nil
        }
    }

    /// `body` as plain text: mentions as "@name", spoilers hidden unless `revealSpoilers` (Copy).
    static func plain(
        _ body: String, ranges: [BodyRange], name: (String) -> String, revealSpoilers: Bool = false
    ) -> String {
        let text = NSMutableString(string: body)
        var end = text.length
        let spoilers =
            revealSpoilers ? [] : ranges.filter { $0.style == .spoiler }.compactMap { clamped($0, to: end) }
        // a mention a hidden spoiler covers goes with it
        let mentions = ranges.filter { r in
            guard case .mention = r.style else { return false }
            return !spoilers.contains { overlap($0, r) }
        }
        // last first; a range overlapping one already replaced is skipped
        let units = Array(body.utf16)
        for r in (spoilers + mentions).sorted(by: { $0.start > $1.start }) {
            let start = Int(r.start)
            let length = Int(r.length)
            guard start + length <= end, length > 0, fits(r, units) else { continue }
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

    /// Whether `r` lies in text of these UTF-16 `units` with neither edge between the halves of a
    /// surrogate pair (an emoji): an index there falls inside a character, where replacing text
    /// traps or leaves half an emoji.
    private static func fits(_ r: BodyRange, _ units: [UInt16]) -> Bool {
        let start = Int(r.start)
        let end = start + Int(r.length)
        guard end <= units.count else { return false }
        return [start, end].allSatisfy { $0 == units.count || !UTF16.isTrailSurrogate(units[$0]) }
    }

    /// `r` cut to a text `length` long; nil if it starts past the end.
    private static func clamped(_ r: BodyRange, to length: Int) -> BodyRange? {
        guard Int(r.start) < length else { return nil }
        return BodyRange(start: r.start, length: UInt32(min(Int(r.length), length - Int(r.start))), style: r.style)
    }

    /// Whether two ranges share a character. In `Int`: a sender's start and length can add up past
    /// `UInt32.max`, which would trap.
    private static func overlap(_ a: BodyRange, _ b: BodyRange) -> Bool {
        Int(a.start) < Int(b.start) + Int(b.length) && Int(b.start) < Int(a.start) + Int(a.length)
    }

    private static func span(_ r: BodyRange?, in text: AttributedString) -> Range<AttributedString.Index>? {
        guard let r, r.length > 0 else { return nil }
        let units = Array(String(text.characters).utf16)
        guard fits(r, units) else { return nil }
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
            return Text("\(text)\(piece)")
        }
    }

    /// Adds `style` to each run in `span`, keeping the styles already there (ranges overlap).
    private static func add(_ style: Styles, to span: Range<AttributedString.Index>, in text: inout AttributedString) {
        let runs = text[span].runs.map { ($0.range, $0[StylesKey.self] ?? []) }
        for (range, current) in runs { text[range][StylesKey.self] = current.union(style) }
    }
}
