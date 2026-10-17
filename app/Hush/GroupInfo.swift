import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// A group's info: its picture, name, description and members, and the changes its access
/// control lets us make. Re-read whenever the chat changes (the core stores each change).
struct GroupInfoView: View {
    let store: AppStore
    let conversation: Conversation
    @Environment(\.dismiss) private var dismiss
    @State private var info: GroupInfo?
    @State private var loaded = false
    @State private var busy = false
    @State private var failure: String?
    @State private var editingDetails = false
    @State private var addingMembers = false
    @State private var confirmingLeave = false
    @State private var removing: GroupMember?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let info {
                ScrollView { content(info).padding(20) }
            } else {
                Text(loaded ? "This group isn't known yet: it shows once a message arrives in it." : "Loading…")
                    .font(.system(size: 13)).foregroundStyle(Theme.secondaryText)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            Divider()
            HStack {
                if busy { ProgressView().controlSize(.small) }
                if let failure {
                    Text(failure).font(.system(size: 12)).foregroundStyle(.red).lineLimit(2)
                }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            .padding(12)
        }
        .frame(width: 400, height: 540)
        // the chat changes with every stored change: read the group again
        .task(id: conversation) { await reload() }
        .sheet(isPresented: $editingDetails) {
            GroupDetailsSheet(title: conversation.title, description: info?.description ?? "") { title, description in
                if title != conversation.title { change(.title(title: title)) }
                if description != info?.description { change(.description(description: description)) }
            }
        }
        .sheet(isPresented: $addingMembers) {
            AddMembersSheet(candidates: store.addableContacts(excluding: Set(info?.members.map(\.aci) ?? []))) {
                change(.addMembers(acis: $0))
            }
        }
        .confirmationDialog(
            "Leave \(store.title(conversation))?", isPresented: $confirmingLeave
        ) {
            Button("Leave Group", role: .destructive) { change(.leave) }
        } message: {
            Text("You won't get its messages any more. The chat stays here.")
        }
        .confirmationDialog(
            "Remove \(removing.map { store.memberName($0.aci) } ?? "")?",
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            presenting: removing
        ) { member in
            Button("Remove from Group", role: .destructive) { change(.removeMember(aci: member.aci)) }
        }
    }

    @ViewBuilder
    private func content(_ info: GroupInfo) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top, spacing: 14) {
                Avatar(id: conversation.id, name: store.title(conversation), size: 64)
                VStack(alignment: .leading, spacing: 4) {
                    Text(store.title(conversation)).font(.system(size: 17, weight: .semibold))
                        .textSelection(.enabled)
                    if !info.description.isEmpty {
                        Text(info.description).font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                            .textSelection(.enabled)
                    }
                }
                Spacer()
            }
            if info.canEditDetails {
                HStack {
                    Button("Edit Name and Description…") { editingDetails = true }
                    Menu("Picture") {
                        Button("Choose…", action: choosePicture)
                        if AvatarPictures.shared.paths[conversation.id] != nil {
                            Button("Remove") { change(.avatar(image: Data())) }
                        }
                    }
                    .fixedSize()
                }
                .disabled(busy)
                Picker(
                    "Disappearing messages",
                    selection: Binding(
                        get: { conversation.expireTimer },
                        set: { change(.timer(seconds: $0)) })
                ) {
                    ForEach(Placeholder.timerChoices(current: conversation.expireTimer), id: \.self) {
                        Text(Placeholder.timerChoice($0)).tag($0)
                    }
                }
                .disabled(busy)
            }
            members(info)
            if !info.invited.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Invited (\(info.invited.count))").font(.system(size: 13, weight: .semibold))
                    ForEach(info.invited, id: \.self) { id in
                        row(id: id, name: store.memberName(id), admin: false)
                    }
                }
            }
            if info.member {
                Button("Leave Group…", role: .destructive) { confirmingLeave = true }
                    .disabled(busy)
            } else {
                Text("You're not in this group any more.").font(.system(size: 12))
                    .foregroundStyle(Theme.secondaryText)
            }
        }
    }

    private func members(_ info: GroupInfo) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("\(info.members.count) members").font(.system(size: 13, weight: .semibold))
                Spacer()
                if info.canAddMembers {
                    Button("Add Members…") { addingMembers = true }.disabled(busy)
                }
            }
            ForEach(sorted(info.members), id: \.aci) { member in
                row(id: member.aci, name: store.memberName(member.aci), admin: member.admin)
                    .contextMenu {
                        // admins manage the others; Signal lets no one demote or remove themselves
                        if info.admin, member.aci != store.ownAci {
                            Button(member.admin ? "Remove as Admin" : "Make Admin") {
                                change(.setAdmin(aci: member.aci, admin: !member.admin))
                            }
                            Button("Remove from Group…", role: .destructive) { removing = member }
                        }
                    }
            }
        }
    }

    /// Us first, then admins, then by name.
    private func sorted(_ members: [GroupMember]) -> [GroupMember] {
        members.sorted { a, b in
            let rank = { (m: GroupMember) in m.aci == store.ownAci ? 0 : m.admin ? 1 : 2 }
            if rank(a) != rank(b) { return rank(a) < rank(b) }
            return store.memberName(a.aci).localizedCaseInsensitiveCompare(store.memberName(b.aci)) == .orderedAscending
        }
    }

    private func row(id: String, name: String, admin: Bool) -> some View {
        HStack(spacing: 10) {
            Avatar(id: "c:\(id)", name: name, size: 28)
            Text(name).font(.system(size: 13)).lineLimit(1)
            Spacer()
            if admin { Text("Admin").font(.system(size: 11)).foregroundStyle(Theme.secondaryText) }
        }
        .accessibilityElement(children: .combine)
    }

    private func reload() async {
        info = await store.groupInfo(conversation.id)
        loaded = true
    }

    private func change(_ change: GroupChange) {
        busy = true
        failure = nil
        Task {
            failure = await store.changeGroup(conversation.id, change)
            busy = false
            // a change that leaves the chat as it was (a description alone) announces nothing
            await reload()
        }
    }

    private func choosePicture() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.image]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        guard let jpeg = GroupPicture.jpeg(from: url) else {
            failure = "That file isn't a picture Hush can read."
            return
        }
        change(.avatar(image: jpeg))
    }
}

/// A group picture as Signal's apps send one: a JPEG at most 1024 px on its longer side.
enum GroupPicture {
    static let maxSide: CGFloat = 1024

    static func jpeg(from url: URL) -> Data? {
        guard let image = NSImage(contentsOf: url),
            let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil)
        else { return nil }
        let scale = min(1, maxSide / CGFloat(max(cg.width, cg.height)))
        let size = NSSize(width: (CGFloat(cg.width) * scale).rounded(), height: (CGFloat(cg.height) * scale).rounded())
        guard
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(size.width), pixelsHigh: Int(size.height), bitsPerSample: 8,
                samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0,
                bitsPerPixel: 0)
        else { return nil }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        NSGraphicsContext.current?.cgContext.draw(cg, in: CGRect(origin: .zero, size: size))
        NSGraphicsContext.restoreGraphicsState()
        return rep.representation(using: .jpeg, properties: [.compressionFactor: 0.85])
    }
}

/// Edits a group's name and description.
struct GroupDetailsSheet: View {
    @State var title: String
    @State var description: String
    let save: (String, String) -> Void
    @Environment(\.dismiss) private var dismiss

    init(title: String, description: String, save: @escaping (String, String) -> Void) {
        _title = State(initialValue: title)
        _description = State(initialValue: description)
        self.save = save
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Name and description").font(.system(size: 17, weight: .semibold))
            TextField("Group name", text: $title).textFieldStyle(.roundedBorder)
            TextField("Description", text: $description, axis: .vertical).textFieldStyle(.roundedBorder)
                .lineLimit(3...6)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Save") {
                    save(title.trimmingCharacters(in: .whitespaces), description.trimmingCharacters(in: .whitespaces))
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(title.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 360)
    }
}

/// Picks people to add to a group from those we have chats with.
struct AddMembersSheet: View {
    let candidates: [(aci: String, name: String)]
    let add: ([String]) -> Void
    @State private var chosen: Set<String> = []
    @State private var filter = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add members").font(.system(size: 17, weight: .semibold))
            TextField("Search", text: $filter).textFieldStyle(.roundedBorder)
            let shown = candidates.filter { filter.isEmpty || $0.name.localizedCaseInsensitiveContains(filter) }
            if shown.isEmpty {
                Text(candidates.isEmpty ? "Everyone you chat with is in this group." : "No one matches.")
                    .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                    .frame(maxWidth: .infinity, minHeight: 200)
            } else {
                List(shown, id: \.aci) { person in
                    Toggle(
                        isOn: Binding(
                            get: { chosen.contains(person.aci) },
                            set: { if $0 { chosen.insert(person.aci) } else { chosen.remove(person.aci) } })
                    ) {
                        HStack(spacing: 10) {
                            Avatar(id: "c:\(person.aci)", name: person.name, size: 24)
                            Text(person.name).lineLimit(1)
                        }
                    }
                }
                .frame(minHeight: 260)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button(chosen.count > 1 ? "Add \(chosen.count)" : "Add") {
                    add(candidates.map(\.aci).filter(chosen.contains))
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(chosen.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}
