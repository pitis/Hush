import AppKit

/// The composer's styled text. Each character's styles live under `stylesKey` and are drawn as
/// fonts, a strikethrough or a spoiler's shade; `message(from:)` turns them into Signal's ranges.
enum ComposerText {
    typealias Styles = MessageText.Styles

    static let stylesKey = NSAttributedString.Key("semaphore.styles")
    /// A mention token: "@name" in the field, the mentioned ACI under this key.
    static let mentionKey = NSAttributedString.Key("semaphore.mention")
    /// The token's text as inserted; once it differs, the token is just text.
    static let mentionTextKey = NSAttributedString.Key("semaphore.mentionText")
    static let mentionColor = NSColor.controlAccentColor
    /// Unique per token, so two tokens side by side (even of one person) stay two runs.
    static let mentionTokenKey = NSAttributedString.Key("semaphore.mentionToken")
    static let size: CGFloat = 14

    /// Each style once, with Signal's name for it.
    static let all: [(Styles, TextStyle)] = [
        (.bold, .bold), (.italic, .italic), (.strikethrough, .strikethrough), (.monospace, .monospace),
        (.spoiler, .spoiler),
    ]

    /// The text to send, trimmed, and its ranges (in UTF-16 units, as `NSString` counts). Each
    /// mention token becomes Signal's placeholder (U+FFFC) under a mention range.
    static func message(from styled: NSAttributedString) -> (body: String, ranges: [BodyRange]) {
        let text = placeholders(styled)
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
        // one placeholder per token now
        text.enumerateAttribute(mentionKey, in: NSRange(location: start, length: length)) { value, run, _ in
            guard let aci = value as? String else { return }
            for i in 0..<run.length {
                ranges.append(
                    BodyRange(start: UInt32(run.location - start + i), length: 1, style: .mention(aci: aci)))
            }
        }
        return (kept, ranges)
    }

    /// `text` with each mention token shrunk to one placeholder character, keeping its styles.
    private static func placeholders(_ text: NSAttributedString) -> NSAttributedString {
        let out = NSMutableAttributedString(attributedString: text)
        var tokens: [NSRange] = []
        text.enumerateAttribute(mentionTokenKey, in: NSRange(location: 0, length: text.length)) { value, run, _ in
            if value != nil { tokens.append(run) }
        }
        for run in tokens.reversed() {
            let attributes = out.attributes(at: run.location, effectiveRange: nil)
            out.replaceCharacters(in: run, with: NSAttributedString(string: "\u{FFFC}", attributes: attributes))
        }
        return out
    }

    /// The "@word" being typed just before `caret` (at the start or after a space), and the word.
    static func mentionQuery(in styled: NSAttributedString, caret: Int) -> (range: NSRange, query: String)? {
        let text = styled.string as NSString
        // a finished mention isn't a query
        if caret > 0, styled.attribute(mentionKey, at: caret - 1, effectiveRange: nil) != nil { return nil }
        var i = caret
        while i > 0 {
            let c = text.character(at: i - 1)
            if c == 0x40 {  // "@"
                let before = i - 1
                if before > 0, let prev = UnicodeScalar(text.character(at: before - 1)),
                    !CharacterSet.whitespacesAndNewlines.contains(prev)
                {
                    return nil
                }
                let range = NSRange(location: before, length: caret - before)
                return (range, text.substring(with: NSRange(location: i, length: caret - i)))
            }
            if let scalar = UnicodeScalar(c), CharacterSet.whitespacesAndNewlines.contains(scalar) { return nil }
            i -= 1
        }
        return nil
    }

    /// Puts a token for `aci`, shown as "@name", and a space in place of `range`, styled like
    /// the text there. Returns where the caret goes.
    @discardableResult
    static func insertMention(aci: String, name: String, in text: NSMutableAttributedString, replacing range: NSRange)
        -> Int
    {
        let styles =
            range.location < text.length
            ? Styles(rawValue: text.attribute(stylesKey, at: range.location, effectiveRange: nil) as? Int ?? 0) : []
        let shown = "@\(name)"
        var attributes = display(styles)
        attributes[mentionKey] = aci
        attributes[mentionTextKey] = shown
        attributes[mentionTokenKey] = UUID().uuidString
        attributes[.foregroundColor] = mentionColor
        let token = NSMutableAttributedString(string: shown, attributes: attributes)
        token.append(NSAttributedString(string: " ", attributes: display(styles)))
        text.replaceCharacters(in: range, with: token)
        return range.location + token.length
    }

    /// Turns tokens whose text was changed back into plain text. True if any was.
    @discardableResult
    static func repairMentions(_ text: NSMutableAttributedString) -> Bool {
        var broken: [NSRange] = []
        text.enumerateAttribute(mentionTokenKey, in: NSRange(location: 0, length: text.length)) { value, run, _ in
            guard value != nil else { return }
            let shown = text.attribute(mentionTextKey, at: run.location, effectiveRange: nil) as? String
            if (text.string as NSString).substring(with: run) != shown { broken.append(run) }
        }
        for run in broken {
            text.removeAttribute(mentionKey, range: run)
            text.removeAttribute(mentionTextKey, range: run)
            text.removeAttribute(mentionTokenKey, range: run)
            text.addAttribute(.foregroundColor, value: NSColor.labelColor, range: run)
        }
        return !broken.isEmpty
    }

    /// The members whose name has a word starting with `query`, by name.
    static func matches(_ members: [(String, String)], query: String) -> [(String, String)] {
        let q = query.lowercased()
        return
            members
            .filter { q.isEmpty || $0.1.lowercased().split(separator: " ").contains { $0.hasPrefix(q) } }
            .sorted { $0.1.localizedCaseInsensitiveCompare($1.1) == .orderedAscending }
    }

    /// `body` with its styles and mentions (named by `name`), to edit.
    static func attributed(
        _ body: String, ranges: [BodyRange], name: (String) -> String = { _ in "" }
    ) -> NSMutableAttributedString {
        let text = NSMutableAttributedString(string: body, attributes: display([]))
        let length = text.length
        let fit = { (r: BodyRange) in r.length > 0 && Int(r.start) + Int(r.length) <= length }
        for r in ranges where fit(r) {
            guard let style = all.first(where: { $0.1 == r.style })?.0 else { continue }
            add(style, in: text, range: NSRange(location: Int(r.start), length: Int(r.length)))
        }
        // last first, so the earlier offsets hold
        let mentions = ranges.filter(fit).compactMap { r -> (Int, String)? in
            guard case .mention(let aci) = r.style, r.length == 1 else { return nil }
            return (Int(r.start), aci)
        }
        for (at, aci) in mentions.sorted(by: { $0.0 > $1.0 }) {
            let shown = "@\(name(aci))"
            var attributes = text.attributes(at: at, effectiveRange: nil)
            attributes[mentionKey] = aci
            attributes[mentionTextKey] = shown
            attributes[mentionTokenKey] = UUID().uuidString
            attributes[.foregroundColor] = mentionColor
            text.replaceCharacters(
                in: NSRange(location: at, length: 1), with: NSAttributedString(string: shown, attributes: attributes))
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
