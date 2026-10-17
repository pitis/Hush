import XCTest

@testable import Hush

final class SearchTextTests: XCTestCase {
    func testMatchesBetweenTheMarkersAreHighlighted() {
        let text = SearchText.highlighted("Let's have \u{E000}lunch\u{E001} \u{E000}tomorrow\u{E001}…")
        XCTAssertEqual(String(text.characters), "Let's have lunch tomorrow…")
        let bold = text.runs.filter { $0.inlinePresentationIntent == .stronglyEmphasized }
            .map { String(text[$0.range].characters) }
        XCTAssertEqual(bold, ["lunch", "tomorrow"])
        // a marker left open doesn't swallow the rest or crash
        XCTAssertEqual(String(SearchText.highlighted("a \u{E000}b").characters), "a b")
    }
}

@MainActor
final class AppStoreSearchTests: XCTestCase {
    private func hit(_ conv: String, _ author: String, _ sentAt: Int64, _ snippet: String = "x") -> SearchHit {
        SearchHit(message: message(conv, author, sentAt, snippet), snippet: snippet)
    }

    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    func testTypingFindsMessagesAndOnlyTheLatestQueryCounts() async {
        let fake = FakeClient(linked: true)
        fake.searchAnswers["lunch"] = [hit("c:a", "a", 2), hit("c:b", "b", 1)]
        let store = await linkedStore(fake)
        store.searchQuery = "lu"
        store.searchQuery = "lunch"  // typed on before the first search ran
        await eventually { store.searchResults.count == 2 }
        XCTAssertEqual(fake.searches.map(\.query), ["lunch"], "searches wait for a pause in typing")
        store.searchQuery = ""
        await eventually { store.searchResults.isEmpty }
    }

    func testANewQueryNeverShowsTheOldOnesResults() async {
        let fake = FakeClient(linked: true)
        fake.searchAnswers["lunch"] = [hit("c:a", "a", 2)]
        let store = await linkedStore(fake)
        store.searchQuery = "lunch"
        await eventually { store.searchResults.count == 1 }
        store.searchQuery = "lunchroom"
        XCTAssertTrue(store.searchResults.isEmpty, "cleared at once, not after the pause")
        XCTAssertTrue(store.searchPending)
        await eventually { !store.searchPending }
        XCTAssertTrue(store.searchResults.isEmpty)
    }

    func testAFailedSearchSaysSo() async {
        let fake = FakeClient(linked: true)
        fake.searchError = TestError(description: "broken")
        let store = await linkedStore(fake)
        store.searchQuery = "lunch"
        await eventually { store.searchFailed != nil }
        XCTAssertTrue(store.searchResults.isEmpty)
        XCTAssertFalse(store.searchPending)
    }

    func testArrowingQuicklyEndsOnTheLastResultPicked() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...300).map { message("c:a", "a", Int64($0)) }
        let far = MessageKey(author: "a", sentAt: 10)
        let near = MessageKey(author: "a", sentAt: 290)
        fake.newerCounts[far] = 290
        fake.newerCounts[near] = 10
        fake.countDelays[far] = .milliseconds(80)  // slower: more to load
        let store = await linkedStore(fake)
        store.select("c:a")
        let (farHit, nearHit) = (hit("c:a", "a", 10), hit("c:a", "a", 290))
        let first = Task { await store.open(farHit) }
        try? await Task.sleep(for: .milliseconds(5))
        let second = Task { await store.open(nearHit) }
        _ = await (first.value, second.value)
        XCTAssertEqual(store.highlighted, near)
        XCTAssertEqual(store.scrollTarget, near)
    }

    func testSearchInChatLooksOnlyThere() async {
        let fake = FakeClient(linked: true)
        fake.conversationAnswers = [.success([conversation("c:a"), conversation("c:b")])]
        fake.searchAnswers["lunch"] = [hit("c:a", "a", 2), hit("c:b", "b", 1)]
        let store = await linkedStore(fake)
        await eventually { store.conversations.count == 2 }
        store.select("c:b")
        store.searchInChat()
        XCTAssertEqual(store.searchScope, "c:b")
        XCTAssertEqual(store.searchFocusRequest, 1)
        store.searchQuery = "lunch"
        await eventually { store.searchResults.map(\.message.conversationId) == ["c:b"] }
        XCTAssertEqual(fake.searches.last?.conversationId, "c:b")
        store.endSearch()
        XCTAssertNil(store.searchScope)
        XCTAssertEqual(store.searchQuery, "")
    }

    func testOpeningAResultLoadsEnoughOfItsChatAndHighlightsIt() async {
        let fake = FakeClient(linked: true)
        fake.messageAnswers["c:a"] = (1...400).map { message("c:a", "a", Int64($0)) }
        let target = MessageKey(author: "a", sentAt: 50)
        fake.newerCounts[target] = 350
        let store = await linkedStore(fake)
        await store.open(hit("c:a", "a", 50))
        XCTAssertEqual(store.selectedId, "c:a")
        await eventually { store.messages.contains { $0.sentAt == 50 } }
        XCTAssertEqual(store.highlighted, target)
        XCTAssertEqual(store.scrollTarget, target)
    }

    func testAResultTooFarBackSaysSo() async {
        let fake = FakeClient(linked: true)
        let target = MessageKey(author: "a", sentAt: 1)
        fake.newerCounts[target] = 50_000
        let store = await linkedStore(fake)
        await store.open(hit("c:a", "a", 1))
        XCTAssertNotNil(store.lastError)
        XCTAssertNil(store.highlighted)
    }
}
