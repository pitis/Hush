import AppKit
import SwiftUI
import XCTest

@testable import Hush

final class ConversationChromeTests: XCTestCase {
    func testTheSubtitleSaysTheMostPressingThingInWords() {
        let subtitle = { (typing: Bool, connection: ConnectionState, timer: Int64, username: String?) in
            ConversationChrome.subtitle(typing: typing, connection: connection, timer: timer, username: username)
        }
        XCTAssertEqual(subtitle(true, .offline, 86_400, "ann.1"), "typing…")
        XCTAssertEqual(subtitle(false, .offline, 86_400, "ann.1"), "Offline")
        XCTAssertEqual(subtitle(false, .connecting, 0, nil), "Connecting…")
        XCTAssertEqual(subtitle(false, .online, 86_400, "ann.1"), "Disappearing messages: 1 day")
        XCTAssertEqual(subtitle(false, .online, 0, "ann.1"), "@ann.1")
        XCTAssertEqual(subtitle(false, .online, 0, nil), "")
    }
}

/// A message row: drawn again only when what it shows changed; the pointer and a right-click count
/// anywhere on it.
@MainActor
final class MessageRowTests: XCTestCase {
    private func row(reactions: [Reaction] = []) -> MessageRow {
        MessageRow(
            message: message("c:a", "a", 1, "hi"), first: true, last: true, isGroup: false, senderName: "A",
            reactions: reactions, files: [], preview: nil, reply: {}, retry: {}, download: { _ in },
            open: { _ in true })
    }

    func testOnlyWhatItShowsCounts() {
        // new closures each time the timeline draws: the same row
        XCTAssertEqual(row(), row())
        XCTAssertNotEqual(row(), row(reactions: [Reaction(reactor: "b", emoji: "👍")]))
    }

    private var window: NSWindow!

    /// The view in a window that's never shown.
    private func host(_ view: some View) -> NSHostingView<AnyView> {
        let hosting = NSHostingView(rootView: AnyView(view.frame(width: 400)))
        window = NSWindow(
            contentRect: NSRect(origin: .zero, size: hosting.fittingSize), styleMask: [.titled], backing: .buffered,
            defer: false)
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        return hosting
    }

    private func click(
        _ type: NSEvent.EventType = .rightMouseDown, at point: NSPoint, _ modifiers: NSEvent.ModifierFlags = []
    ) -> NSEvent {
        NSEvent.mouseEvent(
            with: type, location: point, modifierFlags: modifiers, timestamp: 0, windowNumber: window.windowNumber,
            context: nil, eventNumber: 0, clickCount: 1, pressure: 1)!
    }

    private static func find(in view: NSView) -> NSTextField? {
        if let v = view as? NSTextField { return v }
        return view.subviews.lazy.compactMap { find(in: $0) }.first
    }

    /// The first text field in the view, clicked into as a click does: shown in the window's field
    /// editor, the field its delegate. Its middle, in the window.
    private func clickInto(_ hosting: NSView) throws -> (editor: NSTextView, point: NSPoint) {
        let text = try XCTUnwrap(Self.find(in: hosting))
        let editor = try XCTUnwrap(window.fieldEditor(true, for: text) as? NSTextView)
        text.cell!.select(withFrame: text.bounds, in: text, editor: editor, delegate: text, start: 0, length: 0)
        return (editor, text.convert(NSPoint(x: text.bounds.midX, y: text.bounds.midY), to: nil))
    }

    private static func isMessageMenu(_ menu: NSMenu?) -> Bool {
        menu?.items.contains { $0.title == "Reply" } ?? false
    }

    func testBesideTheBubbleIsStillTheRow() {
        // the empty side, past the react button. The menu stands in for the hover, which can't be
        // driven headless: both come after the row's contentShape
        let hosting = host(row())
        let side = NSPoint(x: hosting.bounds.maxX - 1, y: hosting.bounds.midY)
        XCTAssertTrue(Self.isMessageMenu(hosting.menu(for: click(at: side))))
    }

    func testRightClickOnTextClickedIntoShowsTheMessageMenu() throws {
        let (editor, point) = try clickInto(host(row()))
        // the field editor's own menu: Look Up, Translate…
        XCTAssertFalse(Self.isMessageMenu(editor.menu(for: click(at: point))))
        XCTAssertTrue(Self.isMessageMenu(TimelineList.menuAround(click(at: point))?.menu))
        XCTAssertTrue(Self.isMessageMenu(TimelineList.menuAround(click(.leftMouseDown, at: point, .control))?.menu))
        // a plain click selects as before
        XCTAssertNil(TimelineList.menuAround(click(.leftMouseDown, at: point)))
    }

    func testEditableTextKeepsItsOwnMenu() throws {
        let (_, point) = try clickInto(host(TextField("", text: .constant("hello")).contextMenu { Button("Reply") {} }))
        XCTAssertNil(TimelineList.menuAround(click(at: point)))
    }
}
