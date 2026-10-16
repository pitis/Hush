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
