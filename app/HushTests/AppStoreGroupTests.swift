import XCTest

@testable import Hush

@MainActor
final class AppStoreGroupTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    private let group = "g:" + String(repeating: "ab", count: 32)

    func testAGroupsInfoComesWithItsMembersNamed() async {
        let fake = FakeClient(linked: true)
        fake.titles["c:alice-aci"] = "Alice"
        fake.groupInfoAnswers[group] = GroupInfo(
            description: "Sundays",
            members: [GroupMember(aci: "me-aci", admin: false), GroupMember(aci: "alice-aci", admin: true)],
            invited: [], member: true, admin: false, canEditDetails: true, canAddMembers: true)
        let store = await linkedStore(fake)
        let info = await store.groupInfo(group)
        XCTAssertEqual(info?.description, "Sundays")
        XCTAssertEqual(store.memberName("alice-aci"), "Alice")
        XCTAssertEqual(store.memberName("me-aci"), "You")
    }

    func testAGroupChangeReachesTheCoreAndAFailureIsReturned() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let failure = await store.changeGroup(group, .title(title: "Climbers"))
        XCTAssertNil(failure)
        XCTAssertEqual(fake.groupChanges.map(\.1), [.title(title: "Climbers")])
        fake.changeError = CoreError.Network(message: "offline")
        let refused = await store.changeGroup(group, .leave)
        XCTAssertEqual(refused, "The server couldn't be reached: offline")
    }

    func testOnlyPeopleOutsideTheGroupCanBeAdded() async {
        let fake = FakeClient(linked: true)
        var blocked = conversation("c:blocked-aci")
        blocked.blocked = true
        var request = conversation("c:stranger-aci")
        request.messageRequest = true
        var bob = conversation("c:bob-aci")
        bob.title = "Bob"
        fake.conversationAnswers = [
            .success([
                conversation("c:alice-aci"), bob, conversation("c:me-aci"), conversation("c:PNI:phone"),
                conversation(group), blocked, request,
            ])
        ]
        let store = await linkedStore(fake)
        await eventually { store.conversations.count == 7 }
        let addable = store.addableContacts(excluding: ["alice-aci"])
        XCTAssertEqual(addable.map(\.aci), ["bob-aci"])
        XCTAssertEqual(addable.map(\.name), ["Bob"])
    }
}
