import AppKit
import XCTest

@testable import Semaphore

final class ComposerTextTests: XCTestCase {
    private func range(_ start: UInt32, _ length: UInt32, _ style: TextStyle) -> BodyRange {
        BodyRange(start: start, length: length, style: style)
    }

    private func styled(_ s: String) -> NSMutableAttributedString {
        NSMutableAttributedString(string: s)
    }

    func testAToggledStyleBecomesARangeAndTogglesBackOff() {
        let text = styled("hello world")
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 6, length: 5))
        XCTAssertEqual(ComposerText.message(from: text).ranges, [range(6, 5, .bold)])
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 6, length: 5))
        XCTAssertEqual(ComposerText.message(from: text).ranges, [])
    }

    func testTogglingOverPartlyStyledTextStylesAllOfIt() {
        let text = styled("hello world")
        ComposerText.toggle(.italic, in: text, range: NSRange(location: 0, length: 5))
        ComposerText.toggle(.italic, in: text, range: NSRange(location: 3, length: 8))
        XCTAssertEqual(ComposerText.message(from: text).ranges, [range(0, 11, .italic)])
    }

    func testOverlappingStylesEachGetTheirRange() {
        let text = styled("abcd")
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 0, length: 4))
        ComposerText.toggle(.monospace, in: text, range: NSRange(location: 2, length: 2))
        ComposerText.toggle(.spoiler, in: text, range: NSRange(location: 1, length: 1))
        ComposerText.toggle(.strikethrough, in: text, range: NSRange(location: 3, length: 1))
        XCTAssertEqual(
            Set(ComposerText.message(from: text).ranges.map { "\($0.style) \($0.start) \($0.length)" }),
            ["bold 0 4", "monospace 2 2", "spoiler 1 1", "strikethrough 3 1"])
    }

    func testTrimmingMovesTheRanges() {
        let text = styled("  👋 hi \n")
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 5, length: 2))  // "hi"
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "👋 hi")
        XCTAssertEqual(message.ranges, [range(3, 2, .bold)])
    }

    func testAStyleOnlyOnTrimmedSpaceIsDropped() {
        let text = styled("hi  ")
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 2, length: 2))
        XCTAssertEqual(ComposerText.message(from: text).ranges, [])
    }

    func testAMessagesRangesLoadBackForEditing() {
        let ranges = [range(0, 4, .bold), range(2, 5, .italic), range(5, 2, .spoiler)]
        let text = ComposerText.attributed("bold it", ranges: ranges)
        XCTAssertEqual(text.string, "bold it")
        XCTAssertEqual(
            Set(ComposerText.message(from: text).ranges.map { "\($0.style) \($0.start) \($0.length)" }),
            ["bold 0 4", "italic 2 5", "spoiler 5 2"])
        // ranges that don't fit are left out
        XCTAssertEqual(
            ComposerText.message(from: ComposerText.attributed("hi", ranges: [range(1, 9, .bold)])).ranges, [])
    }

    func testStyledTextIsDrawnWithItsFont() {
        let text = styled("ab")
        ComposerText.toggle(.bold, in: text, range: NSRange(location: 0, length: 1))
        ComposerText.toggle(.monospace, in: text, range: NSRange(location: 1, length: 1))
        let bold = text.attribute(.font, at: 0, effectiveRange: nil) as? NSFont
        let mono = text.attribute(.font, at: 1, effectiveRange: nil) as? NSFont
        XCTAssertTrue(bold?.fontDescriptor.symbolicTraits.contains(.bold) == true)
        XCTAssertTrue(mono?.fontDescriptor.symbolicTraits.contains(.monoSpace) == true)
        ComposerText.toggle(.strikethrough, in: text, range: NSRange(location: 0, length: 2))
        XCTAssertNotNil(text.attribute(.strikethroughStyle, at: 1, effectiveRange: nil))
    }

    func testTheShortcutsAreDesktops() {
        XCTAssertEqual(ComposerText.style(forKey: "b", modifiers: .command), .bold)
        XCTAssertEqual(ComposerText.style(forKey: "i", modifiers: .command), .italic)
        XCTAssertEqual(ComposerText.style(forKey: "x", modifiers: [.command, .shift]), .strikethrough)
        XCTAssertEqual(ComposerText.style(forKey: "e", modifiers: .command), .monospace)
        XCTAssertEqual(ComposerText.style(forKey: "b", modifiers: [.command, .shift]), .spoiler)
        XCTAssertNil(ComposerText.style(forKey: "b", modifiers: []))
        XCTAssertNil(ComposerText.style(forKey: "v", modifiers: .command))
    }
}

final class ComposerMentionTests: XCTestCase {
    private func mention(_ start: UInt32, _ aci: String) -> BodyRange {
        BodyRange(start: start, length: 1, style: .mention(aci: aci))
    }

    func testTheQueryIsTheWordAfterAnAtSignBeforeTheCaret() {
        let q = { (s: String) in
            ComposerText.mentionQuery(in: NSAttributedString(string: s), caret: (s as NSString).length)
        }
        XCTAssertEqual(q("hi @bo")?.query, "bo")
        XCTAssertEqual(q("hi @bo")?.range, NSRange(location: 3, length: 3))
        XCTAssertEqual(q("@")?.query, "")
        XCTAssertNil(q("mail@bo"), "not inside a word")
        XCTAssertNil(q("hi @bo "), "ended by a space")
        XCTAssertNil(q("hi"))
    }

    func testAPickedMentionBecomesARangeOverAPlaceholder() {
        let text = NSMutableAttributedString(string: "hi @bo", attributes: ComposerText.display([]))
        let caret = ComposerText.insertMention(
            aci: "bob", name: "Bob", in: text, replacing: NSRange(location: 3, length: 3))
        XCTAssertEqual(text.string, "hi @Bob ")
        XCTAssertEqual(caret, 8)
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "hi \u{FFFC}")
        XCTAssertEqual(message.ranges, [mention(3, "bob")])
    }

    func testStylesAroundAMentionCountThePlaceholderAsOneUnit() {
        let text = NSMutableAttributedString(string: "@a x", attributes: ComposerText.display([]))
        ComposerText.insertMention(aci: "bob", name: "Bob Smith", in: text, replacing: NSRange(location: 0, length: 2))
        // "@Bob Smith  x": bold over the last word
        ComposerText.toggle(.bold, in: text, range: NSRange(location: text.length - 1, length: 1))
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "\u{FFFC}  x")
        XCTAssertEqual(
            Set(message.ranges.map { "\($0.style) \($0.start) \($0.length)" }),
            ["mention(aci: \"bob\") 0 1", "bold 3 1"])
    }

    func testAChangedMentionIsJustText() {
        let text = NSMutableAttributedString(string: "", attributes: ComposerText.display([]))
        ComposerText.insertMention(aci: "bob", name: "Bob", in: text, replacing: NSRange(location: 0, length: 0))
        text.deleteCharacters(in: NSRange(location: 3, length: 1))  // "@Bo "
        XCTAssertTrue(ComposerText.repairMentions(text))
        XCTAssertEqual(ComposerText.message(from: text).body, "@Bo")
        XCTAssertEqual(ComposerText.message(from: text).ranges, [])
        XCTAssertFalse(ComposerText.repairMentions(text), "nothing left to repair")
    }

    func testAMessagesMentionsLoadBackForEditing() {
        let ranges = [mention(3, "bob"), BodyRange(start: 0, length: 2, style: .italic)]
        let text = ComposerText.attributed("hi \u{FFFC}!", ranges: ranges, name: { $0 == "bob" ? "Bob" : "?" })
        XCTAssertEqual(text.string, "hi @Bob!")
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "hi \u{FFFC}!")
        XCTAssertEqual(Set(message.ranges.map { "\($0.style) \($0.start)" }), ["mention(aci: \"bob\") 3", "italic 0"])
    }

    func testMembersMatchByAnyWordOfTheirName() {
        let members = [("a", "Ana Popescu"), ("b", "Bob"), ("c", "Pop Corn"), ("d", "Dan")]
        XCTAssertEqual(ComposerText.matches(members, query: "pop").map(\.0), ["a", "c"])
        XCTAssertEqual(ComposerText.matches(members, query: "").map(\.0), ["a", "b", "d", "c"])
        XCTAssertEqual(ComposerText.matches(members, query: "zz").map(\.0), [])
    }

    func testTwoMentionsSideBySideStayTwo() {
        let ranges = [mention(0, "bob"), mention(1, "bob")]
        let text = ComposerText.attributed("\u{FFFC}\u{FFFC}", ranges: ranges, name: { _ in "Bob" })
        XCTAssertEqual(text.string, "@Bob@Bob")
        XCTAssertFalse(ComposerText.repairMentions(text))
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "\u{FFFC}\u{FFFC}")
        XCTAssertEqual(message.ranges, ranges)
    }

    func testAStyleSpanningAMentionComesBackWhole() {
        let ranges = [BodyRange(start: 0, length: 5, style: .bold), mention(3, "bob")]
        let text = ComposerText.attributed("hi \u{FFFC}!", ranges: ranges, name: { _ in "Bob" })
        let message = ComposerText.message(from: text)
        XCTAssertEqual(message.body, "hi \u{FFFC}!")
        XCTAssertEqual(Set(message.ranges.map { "\($0.start) \($0.length)" }), ["0 5", "3 1"])
    }

    func testAMentionAfterTrimmedSpaceStartsAtZero() {
        let text = NSMutableAttributedString(string: "  ", attributes: ComposerText.display([]))
        ComposerText.insertMention(aci: "bob", name: "Bob", in: text, replacing: NSRange(location: 2, length: 0))
        XCTAssertEqual(ComposerText.message(from: text).ranges, [mention(0, "bob")])
    }

    func testAFinishedMentionIsntAQuery() {
        let text = NSMutableAttributedString(string: "hi ", attributes: ComposerText.display([]))
        ComposerText.insertMention(aci: "bob", name: "Bob", in: text, replacing: NSRange(location: 3, length: 0))
        text.deleteCharacters(in: NSRange(location: text.length - 1, length: 1))  // the space after it
        XCTAssertNil(ComposerText.mentionQuery(in: text, caret: text.length))
    }
}
