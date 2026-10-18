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
    private func row(reactions: [Reaction] = [], name: String = "Ann") -> MessageRow {
        var m = message("c:a", "a", 1, "hi @ann")
        m.ranges = [BodyRange(start: 3, length: 4, style: .mention(aci: "ann-aci"))]
        return MessageRow(
            message: m, first: true, last: true, isGroup: false, senderName: "A", mentionName: { _ in name },
            reactions: reactions, files: [], preview: nil, reply: {}, retry: {}, download: { _ in },
            open: { _ in true })
    }

    func testOnlyWhatItShowsCounts() {
        // new closures each time the timeline draws: the same row
        XCTAssertEqual(row(), row())
        XCTAssertNotEqual(row(), row(reactions: [Reaction(reactor: "b", emoji: "👍")]))
        // a mentioned person's name came in
        XCTAssertNotEqual(row(name: "a1b2c3d4"), row(name: "Ann"))
    }
}
