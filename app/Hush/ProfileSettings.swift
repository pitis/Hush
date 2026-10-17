import SwiftUI

/// Our name and about in Settings, read from the server each time Settings opens (the phone
/// changes them too). The picture stays the phone's.
struct ProfileSettings: View {
    let store: AppStore
    @State private var saved: OwnProfile?
    @State private var givenName = ""
    @State private var familyName = ""
    @State private var about = ""
    @State private var readFailure: String?
    @State private var saveFailure: String?
    @State private var saving = false

    var body: some View {
        Group {
            if let saved {
                TextField("First name", text: $givenName)
                TextField("Last name", text: $familyName)
                TextField("About", text: $about)
                LabeledContent {
                    HStack {
                        if saving { ProgressView().controlSize(.small) }
                        Button("Save Profile") { save() }
                            .disabled(saving || !changed(from: saved) || ProfileSettings.trimmed(givenName).isEmpty)
                    }
                } label: {
                    Text("Your picture is set on your phone.").font(.caption).foregroundStyle(.secondary)
                }
                if let saveFailure { Text(saveFailure).font(.caption).foregroundStyle(.red) }
            } else if let readFailure {
                LabeledContent("Profile") {
                    Button("Try Again") { Task { await load() } }
                }
                Text(readFailure).font(.caption).foregroundStyle(.secondary)
            } else {
                LabeledContent("Profile") { ProgressView().controlSize(.small) }
            }
        }
        .task { await load() }
    }

    private static func trimmed(_ s: String) -> String { s.trimmingCharacters(in: .whitespacesAndNewlines) }

    private func changed(from saved: OwnProfile) -> Bool {
        OwnProfile(
            givenName: Self.trimmed(givenName), familyName: Self.trimmed(familyName), about: Self.trimmed(about))
            != saved
    }

    private func load() async {
        readFailure = nil
        switch await store.ownProfile() {
        case .profile(let p): show(p)
        case .failed(let why): readFailure = why
        }
    }

    private func show(_ p: OwnProfile) {
        saved = p
        givenName = p.givenName
        familyName = p.familyName
        about = p.about
    }

    private func save() {
        saving = true
        saveFailure = nil
        Task {
            switch await store.saveProfile(givenName: givenName, familyName: familyName, about: about) {
            case .profile(let p): show(p)
            case .failed(let why): saveFailure = why
            }
            saving = false
        }
    }
}
