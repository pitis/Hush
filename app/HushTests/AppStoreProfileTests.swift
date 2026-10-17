import XCTest

@testable import Hush

@MainActor
final class AppStoreProfileTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    func testOurProfileIsReadAndAFailureSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.profile = OwnProfile(givenName: "Ada", familyName: "Lovelace", about: "counting")
        let store = await linkedStore(fake)
        let read = await store.ownProfile()
        XCTAssertEqual(read, .profile(OwnProfile(givenName: "Ada", familyName: "Lovelace", about: "counting")))
        fake.profileError = CoreError.Network(message: "the server answered 500")
        let failed = await store.ownProfile()
        XCTAssertEqual(failed, .failed("Couldn't read your profile: the server answered 500"))
    }

    func testASavedProfileReachesTheCoreAndAFailureSaysWhy() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let saved = await store.saveProfile(givenName: " Ada ", familyName: "Lovelace", about: "counting ")
        XCTAssertEqual(
            saved, .profile(OwnProfile(givenName: "Ada", familyName: "Lovelace", about: "counting")),
            "the core's answer, trimmed")
        XCTAssertEqual(fake.savedProfiles, [OwnProfile(givenName: "Ada", familyName: "Lovelace", about: "counting")])
        fake.profileError = CoreError.InvalidInput(message: "a first name is needed")
        let refused = await store.saveProfile(givenName: " ", familyName: "", about: "")
        XCTAssertEqual(refused, .failed("Couldn't save your profile: a first name is needed"))
    }
}
