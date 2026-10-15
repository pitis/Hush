import XCTest

@testable import Semaphore

@MainActor
final class AppStoreTests: XCTestCase {
    private func bridge(_ fake: FakeClient) -> AppStore {
        AppStore(open: { _ in fake })
    }

    // MARK: boot

    func testBootWhenUnlinkedRequestsALinkCodeWithTheDeviceName() async {
        let fake = FakeClient(linked: false)
        let b = bridge(fake)
        await b.boot()
        XCTAssertEqual(b.screen, .link)
        XCTAssertEqual(fake.linkCalls, ["Semaphore"])
        XCTAssertEqual(fake.startCalls, 0)
    }

    func testBootWhenLinkedStartsReceivingAndShowsChats() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a")])]
        let b = bridge(fake)
        await b.boot()
        XCTAssertEqual(b.screen, .chats)
        XCTAssertEqual(fake.startCalls, 1)
        XCTAssertTrue(fake.linkCalls.isEmpty)
        await eventually { b.conversations.map(\.id) == ["c:a"] }
    }

    func testBootRunsOnce() async {
        let fake = FakeClient(linked: false)
        let opens = Counter()
        let b = AppStore(open: { _ in
            opens.increment()
            return fake
        })
        await b.boot()
        await b.boot()
        XCTAssertEqual(opens.value, 1)
        XCTAssertEqual(fake.linkCalls.count, 1)
    }

    func testBootShowsAlreadyRunningWhenTheDataIsLocked() async {
        let b = AppStore(open: { _ in throw CoreError.AlreadyOpen })
        await b.boot()
        XCTAssertEqual(b.screen, .failed("Semaphore is already running."))
    }

    func testBootShowsAKeychainHintWhenTheKeyIsUnavailable() async {
        let b = AppStore(open: { _ in throw KeychainError.status(errSecInteractionNotAllowed) })
        await b.boot()
        guard case .failed(let message) = b.screen else { return XCTFail("\(b.screen)") }
        XCTAssertTrue(message.contains("keychain"), message)
    }

    func testBootFailsVisiblyWhenLinkingCantStart() async {
        let fake = FakeClient(linked: false)
        fake.linkError = CoreError.Closed
        let b = bridge(fake)
        await b.boot()
        guard case .failed = b.screen else { return XCTFail("\(b.screen)") }
    }

    // MARK: linking

    func testLinkFlowStartsReceivingAfterLinking() async {
        let fake = FakeClient(linked: false)
        let b = bridge(fake)
        await b.boot()
        b.handle(.linkUrl(url: "sgnl://linkdevice?uuid=x"))
        XCTAssertEqual(b.linkURL, "sgnl://linkdevice?uuid=x")
        fake.setLinked(true)
        b.handle(.linked)
        XCTAssertEqual(b.screen, .chats)
        XCTAssertNil(b.linkURL)
        XCTAssertEqual(fake.startCalls, 1)
        b.handle(.connection(state: .connecting))
        b.handle(.connection(state: .online))
        XCTAssertEqual(b.screen, .chats)
        XCTAssertEqual(b.connection, .online)
    }

    func testStartFailureAfterLinkingIsLogged() async {
        let fake = FakeClient(linked: false)
        fake.startError = CoreError.Closed
        let b = bridge(fake)
        await b.boot()
        fake.setLinked(true)
        b.handle(.linked)
        XCTAssertTrue(b.log.last?.text.hasPrefix("start failed:") == true)
    }

    func testExpiredCodeThenRetryShowsAFreshCodeWithoutTheOldError() async {
        let fake = FakeClient(linked: false)
        let b = bridge(fake)
        await b.boot()
        b.handle(.linkUrl(url: "u1"))
        b.retryLink()  // ignored while a code is showing
        XCTAssertEqual(fake.linkCalls.count, 1)
        b.handle(.error(message: "the code expired before it was scanned"))
        b.handle(.connection(state: .unlinked))
        XCTAssertEqual(b.screen, .link)
        XCTAssertNil(b.linkURL)
        XCTAssertEqual(b.lastError, "the code expired before it was scanned")
        b.retryLink()
        XCTAssertNil(b.lastError)
        XCTAssertEqual(fake.linkCalls.count, 2)
        b.handle(.linkUrl(url: "u2"))
        XCTAssertEqual(b.linkURL, "u2")
        XCTAssertEqual(b.screen, .link)
    }

    func testFreshCodeClearsAnEarlierError() async {
        let b = bridge(FakeClient(linked: false))
        await b.boot()
        b.handle(.error(message: "couldn't ask the phone for contacts"))
        b.handle(.linkUrl(url: "u"))
        XCTAssertNil(b.lastError)
    }

    func testRetryFailureShowsTheError() async {
        let fake = FakeClient(linked: false)
        let b = bridge(fake)
        await b.boot()
        b.handle(.connection(state: .unlinked))
        fake.linkError = TestError(description: "boom")
        b.retryLink()
        XCTAssertEqual(b.lastError, "boom")
    }

    func testCoreFailureWhileLinkingStaysOnTheLinkScreenWithTheError() async {
        let b = bridge(FakeClient(linked: false))
        await b.boot()
        b.handle(.linkUrl(url: "u"))
        b.handle(.error(message: "the core stopped unexpectedly; restart Semaphore"))
        b.handle(.connection(state: .offline))
        XCTAssertEqual(b.screen, .link)
        XCTAssertEqual(b.lastError, "the core stopped unexpectedly; restart Semaphore")
    }

    func testFailedScreenIsNotReplacedByConnectionEvents() async {
        let b = AppStore(open: { _ in throw CoreError.AlreadyOpen })
        await b.boot()
        b.handle(.connection(state: .offline))
        XCTAssertEqual(b.screen, .failed("Semaphore is already running."))
    }

    // MARK: conversations

    func testConversationEventsRefreshTheList() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [
            .success([conversation("c:a")]), .success([conversation("c:b", unread: 1), conversation("c:a")]),
        ]
        let b = bridge(fake)
        await b.boot()
        await eventually { b.conversations.map(\.id) == ["c:a"] }
        b.handle(.messageChanged(conversationId: "c:b", key: MessageKey(author: "bbbbbbbb-1", sentAt: 5)))
        await eventually { b.conversations.map(\.id) == ["c:b", "c:a"] }
        XCTAssertEqual(b.conversations.first?.unreadCount, 1)
    }

    func testAFailedRefreshKeepsTheListAndLogs() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a")]), .failure(TestError(description: "locked"))]
        let b = bridge(fake)
        await b.boot()
        await eventually { b.conversations.count == 1 }
        b.handle(.conversationsChanged)
        await eventually { b.log.last?.text == "query failed: locked" }
        XCTAssertEqual(b.conversations.map(\.id), ["c:a"])
    }

    func testABurstOfEventsCoalescesAndTheNewestResultWins() async {
        let fake = FakeClient(linked: true)
        fake.holdCall = 1
        fake.conversationAnswers = [.success([conversation("c:old")]), .success([conversation("c:new")])]
        let b = bridge(fake)
        await b.boot()  // refresh #1, held
        await eventually { fake.isHolding }
        for i in 0..<50 {
            b.handle(.messageChanged(conversationId: "c:new", key: MessageKey(author: "a", sentAt: Int64(i))))
        }
        fake.release()
        await eventually { b.conversations.map(\.id) == ["c:new"] }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.conversationCalls, 2)
        XCTAssertEqual(b.conversations.map(\.id), ["c:new"])
    }

    // MARK: log

    func testLogIsCappedOldestFirstOutAndAllowsRepeats() {
        let b = AppStore(open: { _ in FakeClient(linked: false) })
        for i in 0..<(AppStore.logLimit + 10) {
            b.handle(.error(message: "e\(i)"))
        }
        XCTAssertEqual(b.log.count, AppStore.logLimit)
        XCTAssertEqual(b.log.first?.text, "error: e10")
        b.handle(.error(message: "same"))
        b.handle(.error(message: "same"))
        let last2 = b.log.suffix(2)
        XCTAssertEqual(Set(last2.map(\.text)).count, 1)
        XCTAssertEqual(Set(last2.map(\.id)).count, 2)
        b.handle(.messageChanged(conversationId: "c:abc", key: MessageKey(author: "12345678-aaaa", sentAt: 42)))
        XCTAssertEqual(b.log.last?.text, "message c:abc 12345678 @42")
    }

    func testDataDirectoryIsApplicationSupportSemaphore() throws {
        let dir = try AppStore.dataDirectory()
        XCTAssertEqual(dir.lastPathComponent, "Semaphore")
        XCTAssertEqual(dir.deletingLastPathComponent().lastPathComponent, "Application Support")
    }
}
