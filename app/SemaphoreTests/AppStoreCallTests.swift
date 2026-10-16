import XCTest

@testable import Semaphore

@MainActor
final class AppStoreCallTests: XCTestCase {
    private var active = false

    private func store(_ fake: FakeClient, notifier: FakeNotifier? = nil) async -> AppStore {
        let store = AppStore(open: { _ in fake }, notifier: notifier, isAppActive: { [unowned self] in self.active })
        await store.boot()
        XCTAssertEqual(store.screen, .chats)
        return store
    }

    func testAnIncomingCallRingsThenConnectsThenEnds() async {
        let notifier = FakeNotifier()
        let store = await store(FakeClient(linked: true), notifier: notifier)
        active = false
        store.handle(.callChanged(conversationId: "c:alice", phase: .incoming, reason: nil))
        XCTAssertEqual(store.call?.phase, .incoming)
        XCTAssertEqual(store.call?.incoming, true)
        XCTAssertEqual(notifier.posted.map(\.body), ["Incoming voice call"], "it rings while inactive")
        store.handle(.callChanged(conversationId: "c:alice", phase: .connecting, reason: nil))
        XCTAssertEqual(store.call?.phase, .connecting)
        store.handle(.callChanged(conversationId: "c:alice", phase: .connected, reason: nil))
        XCTAssertNotNil(store.call?.connectedAt, "the duration ticker starts")
        XCTAssertEqual(notifier.removedIds, [Notifications.callId("c:alice")], "the ring stops")
        store.handle(.callChanged(conversationId: "c:alice", phase: .ended, reason: "hung up"))
        XCTAssertEqual(store.call?.phase, .ended)
        XCTAssertEqual(store.call?.endedReason, "hung up")
        store.dismissEndedCall()
        XCTAssertNil(store.call)
    }

    func testAnUnansweredIncomingCallPostsAMissedCallNotification() async {
        let notifier = FakeNotifier()
        let store = await store(FakeClient(linked: true), notifier: notifier)
        active = true  // missed calls notify whatever the focus
        store.handle(.callChanged(conversationId: "c:alice", phase: .incoming, reason: nil))
        store.handle(.callChanged(conversationId: "c:alice", phase: .ended, reason: "hung up"))
        XCTAssertEqual(notifier.posted.map(\.body), ["Missed voice call"])
    }

    func testAConnectedCallNeverReadsAsMissed() async {
        let notifier = FakeNotifier()
        let store = await store(FakeClient(linked: true), notifier: notifier)
        active = true
        store.handle(.callChanged(conversationId: "c:alice", phase: .incoming, reason: nil))
        store.handle(.callChanged(conversationId: "c:alice", phase: .connected, reason: nil))
        store.handle(.callChanged(conversationId: "c:alice", phase: .ended, reason: "hung up"))
        XCTAssertTrue(notifier.posted.filter { $0.body == "Missed voice call" }.isEmpty)
    }

    func testStartCallGuardsAgainstASecondCall() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.handle(.callChanged(conversationId: "c:alice", phase: .connected, reason: nil))
        XCTAssertNil(store.call, "a connect without a ring or start is ignored")
        store.startCall("c:alice")
        XCTAssertEqual(fake.startCallCalls, ["c:alice"])
        store.handle(.callChanged(conversationId: "c:alice", phase: .outgoing, reason: nil))
        store.startCall("c:bob")
        XCTAssertEqual(fake.startCallCalls, ["c:alice"], "no second call while one runs")
    }

    func testAcceptHangupAndMuteReachTheClient() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.handle(.callChanged(conversationId: "c:alice", phase: .incoming, reason: nil))
        store.acceptCall()
        XCTAssertEqual(fake.acceptCallCalls, 1)
        store.toggleCallMuted()
        XCTAssertEqual(fake.setMutedCalls, [true])
        XCTAssertEqual(store.call?.muted, true)
        store.toggleCallMuted()
        XCTAssertEqual(fake.setMutedCalls, [true, false])
        XCTAssertEqual(store.call?.muted, false)
        store.hangupCall()
        XCTAssertEqual(fake.hangupCallCalls, 1)
    }

    func testAFailedMuteKeepsTheShownState() async {
        let fake = FakeClient(linked: true)
        let store = await store(fake)
        store.handle(.callChanged(conversationId: "c:alice", phase: .connected, reason: nil))
        store.handle(.callChanged(conversationId: "c:alice", phase: .incoming, reason: nil))
        fake.callError = TestError(description: "closed")
        store.toggleCallMuted()
        XCTAssertEqual(store.call?.muted, false, "the optimistic flip is taken back")
    }

    func testCallEventWordingCarriesTheDuration() {
        XCTAssertEqual(Placeholder.call("Outgoing voice call", seconds: 125), "Outgoing voice call (2:05)")
        XCTAssertEqual(Placeholder.call("Incoming voice call", seconds: nil), "Incoming voice call")
        XCTAssertEqual(Placeholder.call("Outgoing voice call", seconds: 0), "Outgoing voice call")
    }
}
