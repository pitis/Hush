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

/// A message row is drawn again only when what it shows changed.
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

    /// The row in a window that's never shown.
    private func host(_ row: MessageRow) -> NSHostingView<AnyView> {
        let hosting = NSHostingView(rootView: AnyView(row.frame(width: 400)))
        window = NSWindow(
            contentRect: NSRect(origin: .zero, size: hosting.fittingSize), styleMask: [.titled], backing: .buffered,
            defer: false)
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        return hosting
    }

    private func rightClick(at point: NSPoint) -> NSEvent {
        NSEvent.mouseEvent(
            with: .rightMouseDown, location: point, modifierFlags: [], timestamp: 0, windowNumber: window.windowNumber,
            context: nil, eventNumber: 0, clickCount: 1, pressure: 1)!
    }

    private static func find(in view: NSView) -> NSTextField? {
        if let v = view as? NSTextField { return v }
        return view.subviews.lazy.compactMap { find(in: $0) }.first
    }

    private static func isMessageMenu(_ menu: NSMenu?) -> Bool {
        menu?.items.contains { $0.title == "Reply" } ?? false
    }

    func testBesideTheBubbleIsStillTheRow() {
        // the empty side, past the react button: the hover that shows the button reads the same region
        let hosting = host(row())
        XCTAssertTrue(Self.isMessageMenu(hosting.menu(for: rightClick(at: NSPoint(x: 360, y: hosting.bounds.midY)))))
    }

    func testRightClickOnTextClickedIntoShowsTheMessageMenu() throws {
        let hosting = host(row())
        let text = try XCTUnwrap(Self.find(in: hosting))
        // clicked into: the window's field editor shows the text, with its own menu (Look Up, Translate…)
        let editor = try XCTUnwrap(window.fieldEditor(true, for: text) as? NSTextView)
        text.cell!.select(withFrame: text.bounds, in: text, editor: editor, delegate: text, start: 0, length: 0)
        let event = rightClick(at: text.convert(NSPoint(x: text.bounds.midX, y: text.bounds.midY), to: nil))
        XCTAssertFalse(Self.isMessageMenu(editor.menu(for: event)))
        XCTAssertTrue(Self.isMessageMenu(TimelineList.menuAround(event)?.menu))
    }
}
