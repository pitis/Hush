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
        if !view.attributedString().isEqual(to: text) {
            view.textStorage?.setAttributedString(text)
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
        guard let view = scroll.documentView as? NSTextView, let layout = view.layoutManager,
            let container = view.textContainer
        else { return nil }
        let width = proposal.width ?? 300
        container.containerSize = NSSize(width: width, height: .greatestFiniteMagnitude)
        layout.ensureLayout(for: container)
        let line = layout.defaultLineHeight(for: NSFont.systemFont(ofSize: ComposerText.size))
        let used = layout.usedRect(for: container).height
        return CGSize(width: width, height: min(max(used, line), line * Self.maxLines).rounded(.up))
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerField
        var focusRequest: Int

        init(_ parent: ComposerField) {
            self.parent = parent
            focusRequest = parent.focusRequest
        }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? NSTextView else { return }
            parent.text = NSAttributedString(attributedString: view.attributedString())
        }

        func textView(_ view: NSTextView, doCommandBy selector: Selector) -> Bool {
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

/// Styles with Desktop's shortcuts and the Format menu, pastes text plain and files as files,
/// and shows "Message" while empty.
final class ComposerTextView: NSTextView {
    weak var coordinator: ComposerField.Coordinator?

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard window?.firstResponder === self,
            let style = ComposerText.style(
                forKey: event.charactersIgnoringModifiers ?? "", modifiers: event.modifierFlags)
        else { return super.performKeyEquivalent(with: event) }
        toggle(style)
        return true
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
            let files = try Composer.pastedFiles(from: .general)
            if !files.isEmpty { return coordinator?.parent.onPasteFiles(files) ?? () }
        } catch {
            return coordinator?.parent.onPasteError(error) ?? ()
        }
        // others' fonts and colours stay out: only our styles
        pasteAsPlainText(sender)
    }

    override var readablePasteboardTypes: [NSPasteboard.PasteboardType] { [.string] }

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
