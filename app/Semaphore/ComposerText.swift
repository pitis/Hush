import AppKit

/// The composer's styled text. Each character's styles live under `stylesKey` and are drawn as
/// fonts, a strikethrough or a spoiler's shade; `message(from:)` turns them into Signal's ranges.
enum ComposerText {
    typealias Styles = MessageText.Styles

    static let stylesKey = NSAttributedString.Key("semaphore.styles")
    static let size: CGFloat = 14

    /// Each style once, with Signal's name for it.
    static let all: [(Styles, TextStyle)] = [
        (.bold, .bold), (.italic, .italic), (.strikethrough, .strikethrough), (.monospace, .monospace),
        (.spoiler, .spoiler),
    ]

    /// The text to send, trimmed, and its ranges (in UTF-16 units, as `NSString` counts).
    static func message(from text: NSAttributedString) -> (body: String, ranges: [BodyRange]) {
        let whole = text.string as NSString
        let kept = whole.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !kept.isEmpty else { return ("", []) }
        let start = whole.range(of: kept).location
        let length = (kept as NSString).length
        var ranges: [BodyRange] = []
        for (style, name) in all {
            text.enumerateAttribute(stylesKey, in: NSRange(location: start, length: length)) { value, run, _ in
                guard let raw = value as? Int, Styles(rawValue: raw).contains(style) else { return }
                let from = UInt32(run.location - start)
                // runs of other style mixes split it: join those that touch
                if let last = ranges.last, last.style == name, last.start + last.length == from {
                    ranges[ranges.count - 1].length += UInt32(run.length)
                } else {
                    ranges.append(BodyRange(start: from, length: UInt32(run.length), style: name))
                }
            }
        }
        return (kept, ranges)
    }

    /// `body` with its styles, to edit (mentions aren't kept: they can't be edited here yet).
    static func attributed(_ body: String, ranges: [BodyRange]) -> NSMutableAttributedString {
        let text = NSMutableAttributedString(string: body, attributes: display([]))
        let length = text.length
        for r in ranges {
            guard let style = all.first(where: { $0.1 == r.style })?.0,
                Int(r.start) + Int(r.length) <= length, r.length > 0
            else { continue }
            add(style, in: text, range: NSRange(location: Int(r.start), length: Int(r.length)))
        }
        return text
    }

    /// Turns `style` on over `range`, or off if all of it has it already.
    static func toggle(_ style: Styles, in text: NSMutableAttributedString, range: NSRange) {
        if all(range, of: text, have: style) {
            change(text, range) { $0.subtracting(style) }
        } else {
            add(style, in: text, range: range)
        }
    }

    /// The styles to type with after toggling `style` where the caret is.
    static func toggled(_ style: Styles, typing: [NSAttributedString.Key: Any]) -> [NSAttributedString.Key: Any] {
        let current = Styles(rawValue: typing[stylesKey] as? Int ?? 0)
        return display(current.symmetricDifference(style))
    }

    /// How text with `styles` is drawn (and the styles themselves, under `stylesKey`).
    static func display(_ styles: Styles) -> [NSAttributedString.Key: Any] {
        var font =
            styles.contains(.monospace)
            ? NSFont.monospacedSystemFont(ofSize: size, weight: .regular) : NSFont.systemFont(ofSize: size)
        if styles.contains(.bold) { font = NSFontManager.shared.convert(font, toHaveTrait: .boldFontMask) }
        if styles.contains(.italic) { font = NSFontManager.shared.convert(font, toHaveTrait: .italicFontMask) }
        var attributes: [NSAttributedString.Key: Any] = [
            .font: font, .foregroundColor: NSColor.labelColor, stylesKey: styles.rawValue,
        ]
        if styles.contains(.strikethrough) { attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
        if styles.contains(.spoiler) {
            attributes[.backgroundColor] = NSColor.secondaryLabelColor.withAlphaComponent(0.3)
        }
        return attributes
    }

    /// The style a shortcut toggles, as in Signal Desktop: ⌘B, ⌘I, ⇧⌘X, ⌘E, ⇧⌘B.
    static func style(forKey key: String, modifiers: NSEvent.ModifierFlags) -> Styles? {
        let mods = modifiers.intersection([.command, .shift, .option, .control])
        switch (key.lowercased(), mods) {
        case ("b", .command): return .bold
        case ("i", .command): return .italic
        case ("x", [.command, .shift]): return .strikethrough
        case ("e", .command): return .monospace
        case ("b", [.command, .shift]): return .spoiler
        default: return nil
        }
    }

    private static func add(_ style: Styles, in text: NSMutableAttributedString, range: NSRange) {
        change(text, range) { $0.union(style) }
    }

    private static func all(_ range: NSRange, of text: NSAttributedString, have style: Styles) -> Bool {
        guard range.length > 0 else { return false }
        var every = true
        text.enumerateAttribute(stylesKey, in: range) { value, _, stop in
            if !Styles(rawValue: value as? Int ?? 0).contains(style) {
                every = false
                stop.pointee = true
            }
        }
        return every
    }

    /// Rewrites each run's styles in `range` and how they're drawn.
    private static func change(_ text: NSMutableAttributedString, _ range: NSRange, _ update: (Styles) -> Styles) {
        var runs: [(NSRange, Styles)] = []
        text.enumerateAttribute(stylesKey, in: range) { value, run, _ in
            runs.append((run, update(Styles(rawValue: value as? Int ?? 0))))
        }
        for (run, styles) in runs { text.addAttributes(display(styles), range: run) }
        // a style turned off leaves no strikethrough or shade behind
        for (run, styles) in runs {
            if !styles.contains(.strikethrough) { text.removeAttribute(.strikethroughStyle, range: run) }
            if !styles.contains(.spoiler) { text.removeAttribute(.backgroundColor, range: run) }
        }
    }
}
