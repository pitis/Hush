import AppKit
import SwiftUI

/// The composer's text field: styled text (see `ComposerText`) in an `NSTextView`, up to six
/// lines tall. ↩ sends, ⇧↩ or ⌥↩ breaks the line, ↑ in an empty field edits our last message,
/// ⌘V of files attaches them, and Desktop's shortcuts (or the menu's Format items) style the
/// selection.
struct ComposerField: NSViewRepresentable {
    @Binding var text: NSAttributedString
    /// Changing it puts the caret in the field.
    var focusRequest: Int
    let onReturn: () -> Void
    /// ↑ in the empty field; true if it was used.
    let onUpInEmpty: () -> Bool
    let onPasteFiles: ([URL]) -> Void
    let onPasteError: (Error) -> Void
    /// The group's mention picker; nil in a direct chat.
    var mentions: MentionPicker?

    static let maxLines: CGFloat = 6

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let view = ComposerTextView()
        view.delegate = context.coordinator
        view.coordinator = context.coordinator
        view.isRichText = true
        view.allowsUndo = true
        view.drawsBackground = false
        view.textContainerInset = .zero
        view.textContainer?.lineFragmentPadding = 0
        view.textContainer?.widthTracksTextView = true
        view.isVerticallyResizable = true
        view.isHorizontallyResizable = false
        view.autoresizingMask = [.width]
        view.typingAttributes = ComposerText.display([])
        view.setAccessibilityLabel("Message")
        view.textStorage?.setAttributedString(text)
        mentions?.view = view
        let scroll = NSScrollView()
        scroll.documentView = view
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let view = scroll.documentView as? ComposerTextView else { return }
        mentions?.view = view
        if !view.attributedString().isEqual(to: text) {
            view.textStorage?.setAttributedString(text)
            // typing undone now would land in text that's gone (sent, another chat's draft)
            context.coordinator.undo.removeAllActions()
            if text.length == 0 { view.typingAttributes = ComposerText.display([]) }
            view.setSelectedRange(NSRange(location: text.length, length: 0))
        }
        if context.coordinator.focusRequest != focusRequest {
            context.coordinator.focusRequest = focusRequest
            // once the view is in its window
            DispatchQueue.main.async { view.window?.makeFirstResponder(view) }
        }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView scroll: NSScrollView, context: Context) -> CGSize? {
        guard let view = scroll.documentView as? NSTextView else { return nil }
        let width = proposal.width ?? 300
        // measured on a copy: sizing the view's own container resizes the view, SwiftUI then measures again
        // (at 0, ∞, none and the real width), and the window lays out in a loop until AppKit throws
        let storage = NSTextStorage(attributedString: view.attributedString())
        let layout = NSLayoutManager()
        let container = NSTextContainer(size: NSSize(width: width, height: .greatestFiniteMagnitude))
        container.lineFragmentPadding = 0
        layout.addTextContainer(container)
        storage.addLayoutManager(layout)
        layout.ensureLayout(for: container)
        let line = layout.defaultLineHeight(for: NSFont.systemFont(ofSize: ComposerText.size))
        let used = layout.usedRect(for: container).height
        return CGSize(width: width, height: min(max(used, line), line * Self.maxLines).rounded(.up))
    }

    @MainActor final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerField
        var focusRequest: Int
        /// The field's own: cleared when its text is replaced, without touching the window's.
        let undo: UndoManager

        init(_ parent: ComposerField) {
            self.parent = parent
            focusRequest = parent.focusRequest
            undo = UndoManager()
        }

        func undoManager(for view: NSTextView) -> UndoManager? { undo }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? NSTextView else { return }
            // a mention typed into stops being one
            if let storage = view.textStorage, !view.hasMarkedText() { ComposerText.repairMentions(storage) }
            parent.text = NSAttributedString(attributedString: view.attributedString())
            findMention(in: view)
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard let view = notification.object as? NSTextView else { return }
            // what's typed next to a mention isn't part of it
            if view.typingAttributes[ComposerText.mentionKey] != nil {
                view.typingAttributes[ComposerText.mentionKey] = nil
                view.typingAttributes[ComposerText.mentionTextKey] = nil
            }
            findMention(in: view)
        }

        /// Opens, narrows or closes the picker for the "@word" before the caret.
        private func findMention(in view: NSTextView) {
            guard let mentions = parent.mentions else { return }
            let caret = view.selectedRange()
            let found =
                caret.length == 0 && !view.hasMarkedText()
                ? ComposerText.mentionQuery(in: view.attributedString(), caret: caret.location) : nil
            mentions.update(found)
        }

        func textView(_ view: NSTextView, doCommandBy selector: Selector) -> Bool {
            if let mentions = parent.mentions, mentions.isOpen {
                switch selector {
                case #selector(NSResponder.moveUp(_:)): return mentions.move(-1)
                case #selector(NSResponder.moveDown(_:)): return mentions.move(1)
                case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertTab(_:)):
                    return mentions.pick()
                case #selector(NSResponder.cancelOperation(_:)): return mentions.dismiss()
                default: break
                }
            }
            switch selector {
            case #selector(NSResponder.insertNewline(_:)):
                switch Composer.action(for: NSApp.currentEvent?.modifierFlags ?? []) {
                case .newline: view.insertNewlineIgnoringFieldEditor(nil)
                case .send: parent.onReturn()
                }
                return true
            case #selector(NSResponder.moveUp(_:)):
                return view.string.isEmpty && parent.onUpInEmpty()
            default:
                return false
            }
        }
    }
}

/// The group members matching the "@word" being typed, one of them selected.
@MainActor @Observable
final class MentionPicker {
    /// How many matches show (and can be chosen).
    static let shown = 8
    /// (ACI, name) of who can be mentioned.
    var members: [(String, String)] = []
    private(set) var query: String?
    private(set) var range = NSRange()
    private(set) var selected = 0
    /// Esc closes it until another "@word" is typed.
    private var dismissedAt: Int?
    @ObservationIgnored weak var view: ComposerTextView?

    var matches: [(String, String)] {
        Array((query.map { ComposerText.matches(members, query: $0) } ?? []).prefix(Self.shown))
    }
    var isOpen: Bool { !matches.isEmpty }

    func update(_ found: (range: NSRange, query: String)?) {
        guard let found, found.range.location != dismissedAt else {
            query = nil
            if found == nil { dismissedAt = nil }
            return
        }
        if found.query != query { selected = 0 }
        query = found.query
        range = found.range
    }

    func move(_ step: Int) -> Bool {
        let count = matches.count
        guard count > 0 else { return false }
        selected = (selected + step + count) % count
        return true
    }

    /// Puts the selected (or `index`th) member in the field.
    @discardableResult
    func pick(_ index: Int? = nil) -> Bool {
        let all = matches
        let i = index ?? selected
        guard all.indices.contains(i), let view else { return false }
        view.insertMention(aci: all[i].0, name: all[i].1, replacing: range)
        query = nil
        return true
    }

    func dismiss() -> Bool {
        dismissedAt = range.location
        query = nil
        return true
    }

    /// Another chat: nothing from this one's text carries over.
    func reset() {
        query = nil
        dismissedAt = nil
        members = []
    }
}

/// Styles with Desktop's shortcuts and the Format menu, pastes text plain and files as files,
/// and shows "Message" while empty.
final class ComposerTextView: NSTextView {
    weak var coordinator: ComposerField.Coordinator?
    /// Where ⌘V reads from (tests use their own).
    var pasteboard: NSPasteboard = .general

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard window?.firstResponder === self,
            let style = ComposerText.style(
                forKey: event.charactersIgnoringModifiers ?? "", modifiers: event.modifierFlags)
        else { return super.performKeyEquivalent(with: event) }
        toggle(style)
        return true
    }

    /// Replaces the "@word" at `range` with a mention token, undoably.
    func insertMention(aci: String, name: String, replacing range: NSRange) {
        guard let storage = textStorage, NSMaxRange(range) <= storage.length,
            shouldChangeText(in: range, replacementString: "@\(name) ")
        else { return }
        storage.beginEditing()
        let caret = ComposerText.insertMention(aci: aci, name: name, in: storage, replacing: range)
        storage.endEditing()
        didChangeText()
        setSelectedRange(NSRange(location: caret, length: 0))
    }

    /// Styles the selection, or what's typed next when nothing is selected.
    func toggle(_ style: ComposerText.Styles) {
        let range = selectedRange()
        guard range.length > 0 else {
            typingAttributes = ComposerText.toggled(style, typing: typingAttributes)
            return
        }
        guard let storage = textStorage, shouldChangeText(in: range, replacementString: nil) else { return }
        storage.beginEditing()
        ComposerText.toggle(style, in: storage, range: range)
        storage.endEditing()
        didChangeText()
    }

    override func paste(_ sender: Any?) {
        do {
            let files = try Composer.pastedFiles(from: pasteboard)
            if !files.isEmpty { return coordinator?.parent.onPasteFiles(files) ?? () }
        } catch {
            return coordinator?.parent.onPasteError(error) ?? ()
        }
        // others' fonts and colours stay out: only our styles
        guard let words = pasteboard.string(forType: .string) else { return }
        insertText(words, replacementRange: selectedRange())
    }

    override var readablePasteboardTypes: [NSPasteboard.PasteboardType] { [.string] }

    /// Paste is on for files and a copied image too (a screenshot has no text to paste).
    override func validateUserInterfaceItem(_ item: NSValidatedUserInterfaceItem) -> Bool {
        guard item.action == #selector(paste(_:)) else { return super.validateUserInterfaceItem(item) }
        return pasteboard.availableType(from: [.string, .fileURL]) != nil || NSImage.canInit(with: pasteboard)
    }

    override func menu(for event: NSEvent) -> NSMenu? {
        let menu = super.menu(for: event) ?? NSMenu()
        let format = NSMenu(title: "Format")
        for (title, style, key, shift) in [
            ("Bold", ComposerText.Styles.bold, "b", false), ("Italic", .italic, "i", false),
            ("Strikethrough", .strikethrough, "x", true), ("Monospace", .monospace, "e", false),
            ("Spoiler", .spoiler, "b", true),
        ] {
            let item = NSMenuItem(title: title, action: #selector(formatItem(_:)), keyEquivalent: key)
            item.keyEquivalentModifierMask = shift ? [.command, .shift] : .command
            item.tag = style.rawValue
            item.target = self
            format.addItem(item)
        }
        let item = NSMenuItem(title: "Format", action: nil, keyEquivalent: "")
        item.submenu = format
        menu.insertItem(item, at: 0)
        menu.insertItem(.separator(), at: 1)
        return menu
    }

    @objc private func formatItem(_ item: NSMenuItem) {
        toggle(ComposerText.Styles(rawValue: item.tag))
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard string.isEmpty else { return }
        ("Message" as NSString).draw(
            at: .zero,
            withAttributes: [
                .font: NSFont.systemFont(ofSize: ComposerText.size), .foregroundColor: NSColor.placeholderTextColor,
            ])
    }

    override func didChangeText() {
        super.didChangeText()
        needsDisplay = true  // the placeholder comes and goes
    }
}
