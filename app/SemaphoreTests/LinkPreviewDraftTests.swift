import XCTest

@testable import Semaphore

@MainActor
final class LinkPreviewDraftTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    private func draft(_ url: String) -> PreviewDraft {
        PreviewDraft(url: url, title: "A page", description: nil, image: nil)
    }

    func testTheFirstHttpsLinkIsTheOneAsTyped() {
        XCTAssertEqual(PreviewLink.first(in: "see (https://example.com/a?b=1)."), "https://example.com/a?b=1")
        XCTAssertEqual(PreviewLink.first(in: "http://example.com"), nil)
        XCTAssertEqual(PreviewLink.first(in: "nothing"), nil)
    }

    func testAPreviewIsFetchedForTheLinkTypedAndGoesWithTheMessage() async {
        let fake = FakeClient(linked: true)
        fake.previewDrafts["https://example.com/a"] = draft("https://example.com/a")
        let store = await linkedStore(fake)
        store.select("c:a")
        await store.composerChanged("see https://example.com/a")
        XCTAssertEqual(store.previewDraft?.url, "https://example.com/a")
        await store.send("see https://example.com/a")
        XCTAssertEqual(fake.sentPreviews, [draft("https://example.com/a")])
        XCTAssertNil(store.previewDraft, "used up")
    }

    func testADismissedOrStalePreviewIsntSent() async {
        let fake = FakeClient(linked: true)
        fake.previewDrafts["https://example.com/a"] = draft("https://example.com/a")
        let store = await linkedStore(fake)
        store.select("c:a")
        await store.composerChanged("https://example.com/a")
        store.dismissPreview()
        XCTAssertNil(store.previewDraft)
        // the same link isn't fetched again
        await store.composerChanged("https://example.com/a ok")
        XCTAssertNil(store.previewDraft)
        // the link edited away: no preview goes
        fake.previewDrafts["https://example.com/b"] = draft("https://example.com/b")
        await store.composerChanged("https://example.com/b")
        XCTAssertNotNil(store.previewDraft)
        await store.composerChanged("no link now")
        XCTAssertNil(store.previewDraft)
        await store.send("no link now")
        XCTAssertEqual(fake.sentPreviews, [nil])
    }
}
