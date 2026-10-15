import XCTest

@testable import Semaphore

@MainActor
final class CoreBridgeTests: XCTestCase {
    func testLinkFlow() {
        let bridge = CoreBridge()
        XCTAssertEqual(bridge.screen, .starting)
        bridge.handle(.connection(state: .unlinked))
        XCTAssertEqual(bridge.screen, .link)
        bridge.handle(.linkUrl(url: "sgnl://linkdevice?uuid=x"))
        XCTAssertEqual(bridge.linkURL, "sgnl://linkdevice?uuid=x")
        bridge.handle(.linked)
        XCTAssertEqual(bridge.screen, .chats)
        XCTAssertNil(bridge.linkURL)
    }

    func testConnectionStateIsTracked() {
        let bridge = CoreBridge()
        bridge.handle(.connection(state: .online))
        XCTAssertEqual(bridge.connection, .online)
        XCTAssertEqual(bridge.screen, .chats)
    }

    func testLogKeepsIdsOnlyAndIsCapped() {
        let bridge = CoreBridge()
        for i in 0..<(CoreBridge.logLimit + 10) {
            bridge.handle(.error(message: "e\(i)"))
        }
        XCTAssertEqual(bridge.log.count, CoreBridge.logLimit)
        XCTAssertEqual(bridge.log.last, "error: e\(CoreBridge.logLimit + 9)")
        bridge.handle(
            .messageChanged(
                conversationId: "c:abc", key: MessageKey(author: "12345678-aaaa", sentAt: 42)))
        XCTAssertEqual(bridge.log.last, "message c:abc 12345678 @42")
    }

    func testFailedLinkReturnsToLinkScreenWithError() {
        let bridge = CoreBridge()
        bridge.handle(.linkUrl(url: "sgnl://x"))
        bridge.handle(.error(message: "linking failed: timeout"))
        bridge.handle(.connection(state: .unlinked))
        XCTAssertEqual(bridge.screen, .link)
        XCTAssertNil(bridge.linkURL)
        XCTAssertEqual(bridge.lastError, "linking failed: timeout")
    }
}
