import XCTest

@testable import Semaphore

@MainActor
final class StickerTests: XCTestCase {
    private let fox = StickerPackInfo(
        id: "p1", title: "Foxes", author: "Ana",
        stickers: [StickerInfo(id: 3, emoji: "🦊", path: "/s/p1/3")])

    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    func testOpeningThePickerShowsTheInstalledPacks() async {
        let fake = FakeClient(linked: true)
        fake.stickerPackAnswers = [fox]
        let store = await linkedStore(fake)
        await store.loadStickerPacks()
        XCTAssertEqual(store.stickerPacks, [fox])
    }

    func testAStickerGoesToTheOpenChatWithTheReply() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.select("c:a")
        store.replyTo = message("c:a", "a", 9, "orig")
        await store.sendSticker(pack: "p1", sticker: 3)
        XCTAssertEqual(fake.sentStickers.map(\.conversationId), ["c:a"])
        XCTAssertEqual(fake.sentStickers.first?.packId, "p1")
        XCTAssertEqual(fake.sentStickers.first?.stickerId, 3)
        XCTAssertEqual(fake.sentStickers.first?.quote, Quote(author: "a", sentAt: 9, text: "orig", ranges: []))
        XCTAssertNil(store.replyTo)
    }

    func testAFailedStickerSaysSoAndKeepsTheReply() async {
        let fake = FakeClient(linked: true)
        fake.sendError = CoreError.Network(message: "offline")
        let store = await linkedStore(fake)
        store.select("c:a")
        store.replyTo = message("c:a", "a", 9, "orig")
        await store.sendSticker(pack: "p1", sticker: 3)
        XCTAssertNotNil(store.lastError)
        XCTAssertNotNil(store.replyTo)
    }

    func testAPackLinkAddsThePackOrSaysWhyNot() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        fake.stickerPackAnswers = [fox]
        let added = await store.addStickerPack("https://signal.art/addstickers/#pack_id=a&pack_key=b")
        XCTAssertNil(added)
        XCTAssertEqual(fake.addedStickerLinks, ["https://signal.art/addstickers/#pack_id=a&pack_key=b"])
        XCTAssertEqual(store.stickerPacks, [fox], "shown at once")
        let bad = await store.addStickerPack("bad")
        XCTAssertEqual(bad, "not a sticker pack link")
    }
}
