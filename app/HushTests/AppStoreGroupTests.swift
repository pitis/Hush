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
            invited: ["PNI:bob"], requesting: ["carol-aci"], linkAccess: .approval, link: nil, member: true,
            admin: false, canEditDetails: true, canAddMembers: true)
        let store = await linkedStore(fake)
        guard case .known(let info) = await store.groupInfo(group) else { return XCTFail("read") }
        XCTAssertEqual(info.description, "Sundays")
        XCTAssertEqual(store.memberName("alice-aci"), "Alice")
        XCTAssertEqual(store.memberName("me-aci"), "You")
        XCTAssertEqual(
            fake.titleLookups.sorted(), ["c:PNI:bob", "c:alice-aci", "c:carol-aci"], "invitees and requesters too")
    }

    func testGroupChangesReachTheCoreInOrderAndTheFirstFailureIsKept() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let failure = await store.changeGroup(group, [.title(title: "Climbers"), .description(description: "Sundays")])
        XCTAssertNil(failure)
        XCTAssertEqual(fake.groupChanges.map(\.1), [.title(title: "Climbers"), .description(description: "Sundays")])
        fake.failingGroupChange = .title(title: "Hikers")
        let refused = await store.changeGroup(group, [.title(title: "Hikers"), .description(description: "Saturdays")])
        XCTAssertEqual(refused, "Couldn't change the group: refused", "the later change's success doesn't hide it")
        XCTAssertEqual(fake.groupChanges.last?.1, .description(description: "Saturdays"))
    }

    func testAGroupNotStoredYetIsUnknown() async {
        let store = await linkedStore(FakeClient(linked: true))
        let read = await store.groupInfo(group)
        XCTAssertEqual(read, .unknown)
    }

    func testAGroupThatCantBeReadSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.groupInfoError = CoreError.Storage(message: "locked")
        let store = await linkedStore(fake)
        guard case .failed(let message) = await store.groupInfo(group) else { return XCTFail("read") }
        XCTAssertTrue(message.contains("locked"), message)
    }

    func testOnlyPeopleOutsideTheGroupCanBeAdded() async {
        let fake = FakeClient(linked: true)
        var blocked = conversation("c:blocked-aci")
        blocked.blocked = true
        var request = conversation("c:stranger-aci")
        request.messageRequest = true
        var bob = conversation("c:bob-aci")
        bob.title = "Bob"
        var anna = conversation("c:anna-aci")
        anna.title = "anna"
        fake.conversationAnswers = [
            .success([
                conversation("c:alice-aci"), bob, conversation("c:me-aci"), conversation("c:PNI:phone"),
                conversation(group), blocked, request, anna,
            ])
        ]
        let store = await linkedStore(fake)
        await eventually { store.conversations.count == 8 }
        let addable = store.addableContacts(excluding: ["alice-aci"])
        XCTAssertEqual(addable.map(\.aci), ["anna-aci", "bob-aci"])
        XCTAssertEqual(addable.map(\.name), ["anna", "Bob"], "by name, whatever the case")
    }

    func testANewGroupOpensOnceMade() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let failure = await store.createGroup("Climbers", members: ["alice-aci"])
        XCTAssertNil(failure)
        XCTAssertEqual(fake.createdGroups.map(\.title), ["Climbers"])
        XCTAssertEqual(fake.createdGroups.map(\.members), [["alice-aci"]])
        XCTAssertEqual(store.selectedId, "g:" + String(repeating: "cd", count: 32))
    }

    func testAGroupThatCantBeMadeSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.createGroupError = CoreError.Network(message: "refused")
        let store = await linkedStore(fake)
        let failure = await store.createGroup("Climbers", members: [])
        XCTAssertEqual(failure, "Couldn't make the group: refused")
        XCTAssertNil(store.selectedId)
    }

    func testAcceptingAnInvitationJoinsAndDecliningAlsoDeletesTheChat() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let accepted = await store.answerInvitation(group, accept: true)
        XCTAssertNil(accepted)
        XCTAssertEqual(fake.groupChanges.map(\.1), [.acceptInvitation])
        XCTAssertEqual(fake.flagCalls, [], "an accepted invitation's chat stays")
        let declined = await store.answerInvitation(group, accept: false)
        XCTAssertNil(declined)
        XCTAssertEqual(fake.groupChanges.last?.1, .declineInvitation)
        await eventually { fake.flagCalls == ["delete \(self.group)"] }
    }

    func testAnInvitationNotDeclinedKeepsItsChat() async {
        let fake = FakeClient(linked: true)
        fake.failingGroupChange = .declineInvitation
        let store = await linkedStore(fake)
        let failure = await store.answerInvitation(group, accept: false)
        XCTAssertEqual(failure, "Couldn't decline the invitation: refused")
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.flagCalls, [])
    }

    private let link = "https://signal.group/#CjQKIA"

    func testALinkShowsItsGroupOrWhyNot() async {
        let fake = FakeClient(linked: true)
        let preview = GroupPreview(
            conversationId: group, title: "Hikers", description: "", memberCount: 7, approval: true, requested: false,
            member: false)
        fake.linkPreviews[link] = preview
        let store = await linkedStore(fake)
        let read = await store.previewGroupLink(link)
        XCTAssertEqual(read, .group(preview))
        let bad = await store.previewGroupLink("hello")
        XCTAssertEqual(bad, .failed("Couldn't read the link: that isn't a group link"))
    }

    func testJoiningOpensTheChatAndAFailureSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.joinAnswers[link] = GroupJoined(conversationId: group, requested: true)
        let store = await linkedStore(fake)
        let joined = await store.joinGroup(link)
        XCTAssertNil(joined)
        XCTAssertEqual(store.selectedId, group, "asked: the chat opens, waiting")
        store.select(nil)
        fake.joinError = CoreError.Network(message: "refused")
        let failed = await store.joinGroup(link)
        XCTAssertEqual(failed, "Couldn't join the group: refused")
        XCTAssertNil(store.selectedId)
    }

    func testACancelledRequestTakesItsChatAway() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        let cancelled = await store.cancelJoinRequest(group)
        XCTAssertNil(cancelled)
        XCTAssertEqual(fake.cancelledRequests, [group])
        await eventually { fake.flagCalls == ["delete \(self.group)"] }
        fake.cancelError = CoreError.Network(message: "refused")
        let failed = await store.cancelJoinRequest(group)
        XCTAssertEqual(failed, "Couldn't cancel the request: refused")
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(fake.flagCalls.count, 1, "the chat stays")
    }

    func testAClickedGroupLinkOpensTheJoinSheetAndOthersTheBrowser() async {
        let store = await linkedStore(FakeClient(linked: true))
        XCTAssertTrue(store.handleLink(URL(string: link)!))
        XCTAssertEqual(store.joiningLink, link)
        XCTAssertFalse(store.handleLink(URL(string: "https://example.com/#x")!))
        XCTAssertFalse(store.handleLink(URL(string: "https://signal.group.example.com/#x")!))
    }
}
