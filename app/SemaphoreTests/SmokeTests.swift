import XCTest

@testable import Semaphore

final class SmokeTests: XCTestCase {
    func testCoreVersionIsNotEmpty() {
        XCTAssertFalse(coreVersion().isEmpty)
    }
}
