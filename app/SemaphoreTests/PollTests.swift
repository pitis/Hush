import XCTest

@testable import Semaphore

final class PollTests: XCTestCase {
    private func poll(_ voters: [[String]], multiple: Bool = false, ended: Bool = false) -> Poll {
        Poll(
            options: voters.enumerated().map { PollOption(text: "o\($0.offset)", voters: $0.element) },
            allowMultiple: multiple, ended: ended)
    }

    func testAClickChoosesTakesBackOrSwitchesAsDesktop() {
        // single choice: the click is the choice; on our choice it takes the vote back
        XCTAssertEqual(PollChoice.after(clicking: 1, in: poll([[], []]), me: "me"), [1])
        XCTAssertEqual(PollChoice.after(clicking: 1, in: poll([["me"], []]), me: "me"), [1])
        XCTAssertEqual(PollChoice.after(clicking: 0, in: poll([["me"], []]), me: "me"), [])
        // several: each click toggles that option
        XCTAssertEqual(PollChoice.after(clicking: 1, in: poll([["me"], [], []], multiple: true), me: "me"), [0, 1])
        XCTAssertEqual(PollChoice.after(clicking: 0, in: poll([["me"], ["me"]], multiple: true), me: "me"), [1])
    }

    func testTheSummaryCountsVotersOnceAndSaysWhenItEnded() {
        XCTAssertEqual(PollChoice.summary(poll([["a", "b"], ["a"]], multiple: true)), "2 votes")
        XCTAssertEqual(PollChoice.summary(poll([["a"], []])), "1 vote")
        XCTAssertEqual(PollChoice.summary(poll([[], []], ended: true)), "No votes · Ended")
    }

    @MainActor func testVotingAndEndingAskTheCore() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        let m = message("g:00", "a", 9)
        await store.vote(m, options: [1])
        await store.endPoll(m)
        XCTAssertEqual(fake.flagCalls, ["vote 9 [1]", "end poll 9"])
    }
}
