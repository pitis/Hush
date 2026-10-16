import AppKit
import SwiftUI
import XCTest

@testable import Semaphore

/// The composer's text view in a real window: typing, shortcuts, ↩ and the height.
@MainActor
final class ComposerFieldTests: XCTestCase {
    private final class Box {
        var text = NSAttributedString()
        var returns = 0
        var ups = 0
    }

    private var window: NSWindow!
    private var box: Box!

    private func field(upUsed: Bool = true) -> ComposerField {
        let box = box!
        return ComposerField(
            text: Binding(get: { box.text }, set: { box.text = $0 }), focusRequest: 0,
            onReturn: { box.returns += 1 },
            onUpInEmpty: {
                box.ups += 1
                return upUsed
            },
            onPasteFiles: { _ in }, onPasteError: { _ in })
    }

    /// The text view inside a hosted field, focused.
    private func host(_ field: ComposerField, width: CGFloat = 300) -> (NSHostingView<AnyView>, ComposerTextView) {
        let hosting = NSHostingView(rootView: AnyView(field.frame(width: width)))
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: width, height: 400), styleMask: [.titled], backing: .buffered,
            defer: false)
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        let view = Self.find(in: hosting)!
        window.makeFirstResponder(view)
        return (hosting, view)
    }

    private static func find(in view: NSView) -> ComposerTextView? {
        if let v = view as? ComposerTextView { return v }
        return view.subviews.lazy.compactMap { find(in: $0) }.first
    }

    private func key(_ chars: String, _ modifiers: NSEvent.ModifierFlags) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0, windowNumber: window.windowNumber,
            context: nil, characters: chars, charactersIgnoringModifiers: chars, isARepeat: false, keyCode: 0)!
    }

    override func setUp() {
        box = Box()
    }

    func testTypingReachesTheBindingAndAShortcutStylesTheSelection() {
        let (_, view) = host(field())
        view.insertText("hello world", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(box.text.string, "hello world")
        view.setSelectedRange(NSRange(location: 6, length: 5))
        XCTAssertTrue(view.performKeyEquivalent(with: key("b", .command)))
        XCTAssertEqual(ComposerText.message(from: box.text).ranges, [BodyRange(start: 6, length: 5, style: .bold)])
        // undo takes it back
        view.undoManager?.undo()
        XCTAssertEqual(ComposerText.message(from: box.text).ranges, [])
    }

    func testAShortcutWithNothingSelectedStylesWhatsTypedNext() {
        let (_, view) = host(field())
        view.insertText("a ", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertTrue(view.performKeyEquivalent(with: key("i", .command)))
        view.insertText("b", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(ComposerText.message(from: box.text).ranges, [BodyRange(start: 2, length: 1, style: .italic)])
    }

    func testReturnSendsAndUpEditsOnlyWhenEmpty() {
        let (_, view) = host(field())
        XCTAssertTrue(view.delegate!.textView!(view, doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(box.returns, 1)
        XCTAssertTrue(view.delegate!.textView!(view, doCommandBy: #selector(NSResponder.moveUp(_:))))
        XCTAssertEqual(box.ups, 1)
        view.insertText("x", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertFalse(view.delegate!.textView!(view, doCommandBy: #selector(NSResponder.moveUp(_:))))
        XCTAssertEqual(box.ups, 1, "not asked with text in the field")
    }

    func testTheFieldGrowsToSixLinesThenScrolls() {
        let (hosting, view) = host(field())
        let one = hosting.fittingSize.height
        view.insertText("a\nb\nc", replacementRange: NSRange(location: NSNotFound, length: 0))
        hosting.rootView = AnyView(field().frame(width: 300))
        hosting.layoutSubtreeIfNeeded()
        let three = hosting.fittingSize.height
        XCTAssertEqual(three / one, 3, accuracy: 0.2)
        view.insertText("\nd\ne\nf\ng\nh\ni", replacementRange: NSRange(location: NSNotFound, length: 0))
        hosting.rootView = AnyView(field().frame(width: 300))
        hosting.layoutSubtreeIfNeeded()
        XCTAssertEqual(hosting.fittingSize.height / one, 6, accuracy: 0.2)
    }

    func testTextSetFromOutsideShowsAndClearsWithPlainTypingAfter() {
        box.text = ComposerText.attributed("edit me", ranges: [BodyRange(start: 0, length: 4, style: .bold)])
        let (hosting, view) = host(field())
        XCTAssertEqual(view.string, "edit me")
        box.text = NSAttributedString()
        hosting.rootView = AnyView(field().frame(width: 300))
        hosting.layoutSubtreeIfNeeded()
        XCTAssertEqual(view.string, "")
        view.insertText("plain", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(ComposerText.message(from: box.text).ranges, [])
    }
}
