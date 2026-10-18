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
