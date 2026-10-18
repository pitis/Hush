import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Backup files: one file per backup, `.hushbackup`.
enum BackupFile {
    static let type = UTType(filenameExtension: "hushbackup", conformingTo: .data) ?? .data

    static func defaultName(on date: Date = .now) -> String {
        "Hush Backup \(date.formatted(.iso8601.year().month().day())).hushbackup"
    }
}

/// Settings ▸ Backup: every chat and file here into one encrypted file.
struct BackupSettings: View {
    let store: AppStore
    @State private var asking = false

    var body: some View {
        LabeledContent("Backup") {
            Button("Back Up…") { asking = true }
        }
        .sheet(isPresented: $asking) { BackupSheet(store: store) }
    }
}

private struct BackupSheet: View {
    let store: AppStore
    @Environment(\.dismiss) private var dismiss
    @State private var passphrase = ""
    @State private var again = ""
    @State private var working = false
    @State private var failure: String?
    @State private var saved: URL?

    private var acceptable: Bool { passphrase.count >= 8 && again == passphrase }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Back Up Your Chats").font(.headline)
            if let saved {
                Text("Backed up to \(saved.lastPathComponent).")
                HStack {
                    Spacer()
                    Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
                }
            } else {
                Text(
                    "Every chat and file on this Mac goes into one file, encrypted with a passphrase. On a new "
                        + "Mac, restore it before linking. Without the passphrase, nobody can open it, you included."
                )
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                SecureField("Passphrase (at least 8 characters)", text: $passphrase).disabled(working)
                SecureField("Passphrase again", text: $again).disabled(working)
                if let failure { Text(failure).font(.callout).foregroundStyle(.red) }
                HStack {
                    Spacer()
                    if working { ProgressView().controlSize(.small) }
                    Button("Cancel", role: .cancel) { dismiss() }.disabled(working)
                    Button("Back Up…", action: save).keyboardShortcut(.defaultAction)
                        .disabled(working || !acceptable)
                }
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func save() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = BackupFile.defaultName()
        panel.allowedContentTypes = [BackupFile.type]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        working = true
        failure = nil
        Task {
            failure = await store.backUp(to: url, passphrase: passphrase)
            working = false
            if failure == nil { saved = url }
        }
    }
}

/// The link screen's Restore from a Backup…: picks the file, then asks for its passphrase.
struct RestoreButton: View {
    let restore: (URL, String) async -> String?
    @State private var file: PickedFile?

    var body: some View {
        Button("Restore from a Backup…", action: pick)
            .buttonStyle(.link)
            .sheet(item: $file) { RestoreSheet(file: $0.url, restore: restore) }
    }

    private func pick() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [BackupFile.type]
        if panel.runModal() == .OK, let url = panel.url { file = PickedFile(url: url) }
    }
}

private struct PickedFile: Identifiable {
    let url: URL
    var id: URL { url }
}

private struct RestoreSheet: View {
    let file: URL
    let restore: (URL, String) async -> String?
    @Environment(\.dismiss) private var dismiss
    @State private var passphrase = ""
    @State private var working = false
    @State private var failure: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Restore \(file.lastPathComponent)?").font(.headline)
            Text(
                "Its chats and files replace any on this Mac. Then link this Mac to your phone: history the "
                    + "phone transfers is merged in."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            SecureField("Passphrase", text: $passphrase).disabled(working)
            if let failure { Text(failure).font(.callout).foregroundStyle(.red) }
            HStack {
                Spacer()
                if working { ProgressView().controlSize(.small) }
                Button("Cancel", role: .cancel) { dismiss() }.disabled(working)
                Button("Restore", action: go).keyboardShortcut(.defaultAction)
                    .disabled(working || passphrase.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func go() {
        working = true
        failure = nil
        Task {
            failure = await restore(file, passphrase)
            working = false
            if failure == nil { dismiss() }
        }
    }
}
