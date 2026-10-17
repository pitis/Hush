import XCTest

@testable import Hush

final class SmokeTests: XCTestCase {
    func testCoreVersionIsNotEmpty() {
        XCTAssertFalse(coreVersion().isEmpty)
    }
}
