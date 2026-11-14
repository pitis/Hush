import XCTest

@testable import Hush

@MainActor
final class SafetyNumberTests: XCTestCase {
    func testTheDigitsShowAsTwelveGroupsOfFiveInThreeRows() {
        let digits = "300354477692869396892869876765458257569162576843440918079131"
        XCTAssertEqual(
            SafetyNumberView.formatted(digits),
            "30035 44776 92869 39689\n28698 76765 45825 75691\n62576 84344 09180 79131")
    }

    func testTheNumberComesFromTheCoreOrSaysWhyNot() async {
        let fake = FakeClient(linked: true)
        let number = SafetyNumber(digits: String(repeating: "1", count: 60), scannable: Data([8, 2]))
        fake.safetyNumbers["alice-aci"] = number
        let store = AppStore(open: { _ in fake })
        await store.boot()
        let alice = await store.safetyNumber("alice-aci")
        XCTAssertEqual(alice, .known(number))
        let stranger = await store.safetyNumber("bob-aci")
        XCTAssertEqual(stranger, .unknown, "no session with them yet")
    }
}
