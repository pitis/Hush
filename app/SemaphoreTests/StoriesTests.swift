import SwiftUI
import XCTest

@testable import Semaphore

final class StoriesTests: XCTestCase {
    private func story(_ author: String, _ conversation: String, _ sentAt: Int64, viewed: Bool = false) -> Story {
        Story(conversationId: conversation, author: author, sentAt: sentAt, text: nil, file: nil, viewed: viewed)
    }

    func testStoriesGroupByChatOursFirstThenUnseenThenNewest() {
        let stories = [
            story("a", "c:a", 1, viewed: true), story("b", "c:b", 2), story("a", "c:a", 3, viewed: true),
            story("me", "c:me", 4, viewed: true), story("b", "g:00", 5),
        ]
        let groups = StoryList.groups(stories, me: "me")
        XCTAssertEqual(groups.map(\.conversationId), ["c:me", "g:00", "c:b", "c:a"])
        XCTAssertEqual(groups.last?.stories.map(\.sentAt), [1, 3], "oldest first within a chat")
        XCTAssertEqual(groups.map(\.unseen), [false, true, true, false])
        XCTAssertEqual(StoryList.unseenCount(stories, me: "me"), 2, "ours never count")
    }

    func testSeeingAStoryDoesntMoveItsChatWhileTheWindowIsOpen() {
        let before = StoryList.groups([story("a", "c:a", 1), story("b", "c:b", 2)], me: "me")
        XCTAssertEqual(before.map(\.id), ["c:b", "c:a"])
        // b's seen now (it would sort after a), and c came meanwhile
        let after = StoryList.groups(
            [story("a", "c:a", 1), story("b", "c:b", 2, viewed: true), story("c", "c:c", 3)], me: "me")
        XCTAssertEqual(StoryList.ordered(after, as: before.map(\.id)).map(\.id), ["c:b", "c:a", "c:c"])
    }

    func testAColourComesFromItsARGBNumber() {
        let c = StoryList.rgba(0x80FF_0010)
        XCTAssertEqual(c.alpha, 128.0 / 255, accuracy: 0.001)
        XCTAssertEqual(c.red, 1, accuracy: 0.001)
        XCTAssertEqual(c.green, 0, accuracy: 0.001)
        XCTAssertEqual(c.blue, 16.0 / 255, accuracy: 0.001)
    }

    @MainActor func testStoriesLoadAndSeenOnesAreMarked() async {
        let fake = FakeClient(linked: true)
        fake.storyAnswers = [story("b", "c:b", 2)]
        let store = AppStore(open: { _ in fake })
        await store.boot()
        fake.titles["c:b"] = "Bob"
        await store.refreshStories()
        XCTAssertEqual(store.stories.map(\.sentAt), [2])
        XCTAssertEqual(store.senderName("b"), "Bob", "authors are named")
        await store.storyViewed(store.stories[0])
        XCTAssertEqual(fake.viewedStories, [MessageKey(author: "b", sentAt: 2)])
        XCTAssertTrue(store.stories[0].viewed)
    }
}
