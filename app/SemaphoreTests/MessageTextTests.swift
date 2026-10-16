import SwiftUI
import XCTest

@testable import Semaphore

final class MessageTextTests: XCTestCase {
    private let names = ["bob": "Bob", "me": "You"]
    private func name(_ aci: String) -> String { names[aci] ?? "?" }

    private func range(_ start: UInt32, _ length: UInt32, _ style: TextStyle) -> BodyRange {
        BodyRange(start: start, length: length, style: style)
    }

    func testMentionsReadAsAtNameInPlainText() {
        let body = "hi \u{FFFC}, and \u{FFFC}"
        let ranges = [range(3, 1, .mention(aci: "bob")), range(10, 1, .mention(aci: "me"))]
        XCTAssertEqual(MessageText.plain(body, ranges: ranges, name: name), "hi @Bob, and @You")
    }

    func testSpoilersAreHiddenInPlainText() {
        XCTAssertEqual(
            MessageText.plain("the end: he dies", ranges: [range(9, 7, .spoiler)], name: name), "the end: ■■■■")
    }

    func testOffsetsCountUTF16LikeSignal() {
        // 👍 is two UTF-16 units: the mention starts at 3, not 2
        let body = "👍 \u{FFFC}"
        XCTAssertEqual(MessageText.plain(body, ranges: [range(3, 1, .mention(aci: "bob"))], name: name), "👍 @Bob")
    }

    func testRangesOutsideTheTextAreIgnored() {
        XCTAssertEqual(MessageText.plain("hi", ranges: [range(1, 9, .mention(aci: "bob"))], name: name), "hi")
        XCTAssertEqual(String(MessageText.attributed("hi", ranges: [range(5, 1, .bold)], name: name).characters), "hi")
    }

    /// Each run's text, styles, and whether it is struck through.
    private func runs(_ text: AttributedString) -> [(String, MessageText.Styles, Bool)] {
        text.runs.map {
            (String(text[$0.range].characters), $0[MessageText.StylesKey.self] ?? [], $0.strikethroughStyle != nil)
        }
    }

    func testStylesOverlap() {
        let text = MessageText.attributed(
            "bold italic", ranges: [range(0, 4, .bold), range(0, 11, .italic), range(5, 6, .strikethrough)],
            name: name)
        let r = runs(text)
        XCTAssertEqual(r.map { $0.0 }, ["bold", " ", "italic"])
        XCTAssertEqual(r.map { $0.1 }, [[.bold, .italic], [.italic], [.italic, .strikethrough]])
        XCTAssertEqual(r.map { $0.2 }, [false, false, true])
    }

    func testMonospace() {
        let r = runs(MessageText.attributed("run ls", ranges: [range(4, 2, .monospace)], name: name))
        XCTAssertEqual(r.map { $0.0 }, ["run ", "ls"])
        XCTAssertEqual(r.map { $0.1 }, [[], [.monospace]])
    }

    func testAMentionShowsTheNameInBoldAndKeepsTheTextAroundIt() {
        let text = MessageText.attributed(
            "\u{FFFC} look", ranges: [range(0, 1, .mention(aci: "bob")), range(2, 4, .italic)], name: name)
        XCTAssertEqual(String(text.characters), "@Bob look")
        XCTAssertEqual(runs(text).map { $0.1 }, [[.bold], [], [.italic]])
    }

    func testASpoilerIsHiddenUntilRevealed() {
        let hidden = MessageText.attributed("psst", ranges: [range(0, 4, .spoiler)], name: name)
        XCTAssertNotNil(hidden.runs.first?.backgroundColor)
        let shown = MessageText.attributed("psst", ranges: [range(0, 4, .spoiler)], name: name, revealSpoilers: true)
        XCTAssertNil(shown.runs.first?.backgroundColor)
        XCTAssertEqual(String(shown.characters), "psst")
        XCTAssertTrue(MessageText.hasSpoiler([range(0, 4, .spoiler)]))
        XCTAssertFalse(MessageText.hasSpoiler([range(0, 4, .bold)]))
    }

    func testMentionedPeopleAreListedForNameLookup() {
        XCTAssertEqual(
            MessageText.mentioned([
                range(0, 1, .mention(aci: "bob")), range(1, 1, .bold), range(2, 1, .mention(aci: "bob")),
            ]),
            ["bob"])
    }

    func testTheChatListResolvesMentionsAndHidesSpoilers() {
        var c = conversation("c:a")
        c.lastMessagePreview = "\u{FFFC} secret"
        c.lastMessageRanges = [range(0, 1, .mention(aci: "bob")), range(2, 6, .spoiler)]
        XCTAssertEqual(
            ChatList.line(c, selected: false, typing: false, draft: "", authorName: "A", name: name),
            .message("@Bob ■■■■", tick: nil))
    }

    func testAMentionInsideASpoilerStaysHiddenInPlainText() {
        // inside, and sharing the spoiler's start
        XCTAssertEqual(
            MessageText.plain(
                "a \u{FFFC} b", ranges: [range(0, 5, .spoiler), range(2, 1, .mention(aci: "bob"))], name: name),
            "■■■■")
        XCTAssertEqual(
            MessageText.plain(
                "\u{FFFC} b", ranges: [range(0, 1, .mention(aci: "bob")), range(0, 3, .spoiler)], name: name),
            "■■■■")
        // revealed (Copy): the mention reads as usual
        XCTAssertEqual(
            MessageText.plain(
                "a \u{FFFC} b", ranges: [range(0, 5, .spoiler), range(2, 1, .mention(aci: "bob"))], name: name,
                revealSpoilers: true),
            "a @Bob b")
    }

    func testAMentionInsideAHiddenSpoilerIsCoveredAndKeepsTheStylesAroundIt() {
        let text = MessageText.attributed(
            "a \u{FFFC} b", ranges: [range(0, 5, .spoiler), range(0, 5, .italic), range(2, 1, .mention(aci: "bob"))],
            name: name)
        XCTAssertEqual(String(text.characters), "a @Bob b")
        XCTAssertTrue(text.runs.allSatisfy { $0.backgroundColor != nil })
        XCTAssertEqual(runs(text).map { $0.1 }, [[.italic], [.italic, .bold], [.italic]])
    }

    func testASpoilerRunningPastTheTextStillHidesToTheEnd() {
        XCTAssertEqual(MessageText.plain("hi there", ranges: [range(3, 50, .spoiler)], name: name), "hi ■■■■")
        let text = MessageText.attributed("hi there", ranges: [range(3, 50, .spoiler)], name: name)
        XCTAssertEqual(
            text.runs.compactMap { $0.backgroundColor == nil ? nil : String(text[$0.range].characters) }, ["there"])
    }

    func testStylesCountUTF16InBubblesToo() {
        let text = MessageText.attributed("👍 hi", ranges: [range(3, 2, .bold)], name: name)
        XCTAssertEqual(runs(text).map { $0.0 }, ["👍 ", "hi"])
        XCTAssertEqual(runs(text).map { $0.1 }, [[], [.bold]])
    }

    /// Ranges come from the sender: no values may crash the app.
    func testHostileRangesDontCrash() {
        let max = UInt32.max
        let hostile = [
            range(max, max, .spoiler), range(max, 2, .mention(aci: "bob")), range(1, max, .mention(aci: "bob")),
            range(max - 1, 2, .bold), range(0, max, .spoiler), range(2, max, .italic),
        ]
        XCTAssertEqual(MessageText.plain("hi \u{FFFC}", ranges: hostile, name: name), "■■■■")
        XCTAssertEqual(
            MessageText.plain("hi \u{FFFC}", ranges: hostile, name: name, revealSpoilers: true), "hi \u{FFFC}")
        XCTAssertEqual(
            String(MessageText.attributed("hi \u{FFFC}", ranges: hostile, name: name).characters), "hi \u{FFFC}")
    }

    /// A range edge inside an emoji (between the halves of a UTF-16 surrogate pair) is ignored:
    /// replacing text there traps.
    func testRangesSplittingAnEmojiAreIgnored() {
        for body in ["👍", "👍👍 hi", "🇷🇴🇩🇪", "👨‍👩‍👧 z", "a👍b"] {
            for start: UInt32 in 0...4 {
                for length: UInt32 in 1...3 {
                    let ranges = [range(start, length, .mention(aci: "bob")), range(start, length, .spoiler)]
                    _ = MessageText.attributed(body, ranges: ranges, name: name)
                    _ = MessageText.attributed(body, ranges: [range(start, length, .mention(aci: "bob"))], name: name)
                    _ = MessageText.plain(body, ranges: ranges, name: name)
                }
            }
        }
        XCTAssertEqual(
            String(MessageText.attributed("👍", ranges: [range(1, 1, .mention(aci: "bob"))], name: name).characters), "👍"
        )
        XCTAssertEqual(MessageText.plain("👍", ranges: [range(1, 1, .mention(aci: "bob"))], name: name), "👍")
    }
}
