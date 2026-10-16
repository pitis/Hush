import AppKit
import SwiftUI

struct ComposerView: View {
    let store: AppStore
    let conversationId: String
    @State private var text = NSAttributedString()
    /// Bumped to put the caret in the field.
    @State private var focusRequest = 0
    /// What was typed before editing started; back once the edit is saved or cancelled.
    @State private var draftBeforeEdit: NSAttributedString?
    @State private var mentions = MentionPicker()

    var body: some View {
        VStack(spacing: 6) {
            if mentions.isOpen { MentionList(picker: mentions) }
            if let reply = store.replyTo {
                HStack {
                    Rectangle().fill(Theme.outgoing).frame(width: 3)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(
                            reply.isOutgoing ? "Replying to yourself" : "Replying to \(store.senderName(reply.author))"
                        )
                        .font(.system(size: 11, weight: .semibold))
                        Text(
                            reply.body.map { MessageText.plain($0, ranges: reply.ranges, name: store.mentionName) }
                                ?? "Attachment"
                        )
                        .font(.system(size: 12)).lineLimit(1)
                        .foregroundStyle(Theme.secondaryText)
                    }
                    Spacer()
                    Button {
                        store.replyTo = nil
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                    }
                    .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                    .keyboardShortcut(.cancelAction)
                }
                .frame(height: 34)
                .padding(.horizontal, 12)
            }
            if let editing = store.editing {
                HStack {
                    Rectangle().fill(Theme.outgoing).frame(width: 3)
                    VStack(alignment: .leading, spacing: 1) {
                        Text("Editing message").font(.system(size: 11, weight: .semibold))
                        Text(
                            editing.body.map { MessageText.plain($0, ranges: editing.ranges, name: store.mentionName) }
                                ?? ""
                        )
                        .font(.system(size: 12)).lineLimit(1)
                        .foregroundStyle(Theme.secondaryText)
                    }
                    Spacer()
                    Button(action: store.cancelEditing) { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                        .keyboardShortcut(.cancelAction)
                        .accessibilityLabel("Cancel editing")
                }
                .frame(height: 34)
                .padding(.horizontal, 12)
            }
            if !store.outgoingFiles.isEmpty { FileTray(store: store) }
            HStack(alignment: .bottom, spacing: 10) {
                Button(action: chooseFiles) {
                    Image(systemName: "paperclip").font(.system(size: 18))
                }
                .accessibilityLabel("Attach files")
                .disabled(store.editing != nil)
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .padding(.bottom, 9)
                Button {
                    focusRequest += 1  // the palette types into the focused field
                    DispatchQueue.main.async { NSApp.orderFrontCharacterPalette(nil) }
                } label: {
                    Image(systemName: "face.smiling").font(.system(size: 20))
                }
                .accessibilityLabel("Emoji")
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .padding(.bottom, 8)
                ComposerField(
                    text: $text, focusRequest: focusRequest, onReturn: submit,
                    // ↑ in an empty field edits your last message, as in Desktop
                    onUpInEmpty: { store.editing == nil && store.editLast() },
                    onPasteFiles: { store.addFiles($0.map(Files.outgoing)) },
                    onPasteError: { store.showError("Couldn't attach what you pasted: \($0.localizedDescription)") },
                    mentions: conversationId.hasPrefix("g:") ? mentions : nil
                )
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                .background(Theme.field, in: RoundedRectangle(cornerRadius: 18))
            }
            .padding(.horizontal, 14)
            .padding(.bottom, 12)
            .padding(.top, store.replyTo == nil ? 8 : 0)
        }
        .onAppear {
            text = Self.plain(Composer.draft(for: conversationId))
            takeFocusIfAsked()
        }
        .task(id: conversationId) { mentions.members = await store.mentionCandidates(conversationId) }
        .onChange(of: conversationId) { _, new in
            // an edit doesn't follow to another chat, nor does the draft it put aside
            draftBeforeEdit = nil
            text = Self.plain(Composer.draft(for: new))
        }
        .onChange(of: text) { _, new in
            // drafts keep the words, not their styles
            if store.editing == nil { Composer.saveDraft(new.string, for: conversationId) }
        }
        .onChange(of: store.editing?.sentAt) { _, _ in
            if let editing = store.editing {
                if draftBeforeEdit == nil { draftBeforeEdit = text }
                text = ComposerText.attributed(editing.body ?? "", ranges: editing.ranges, name: store.mentionName)
            } else if let draft = draftBeforeEdit {
                text = draft
                draftBeforeEdit = nil
            }
        }
        .onChange(of: store.composerWantsFocus) { _, _ in takeFocusIfAsked() }
    }

    private func takeFocusIfAsked() {
        guard store.composerWantsFocus else { return }
        focusRequest += 1
        store.composerTookFocus()
    }

    private func chooseFiles() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        if panel.runModal() == .OK { store.addFiles(Files.attachable(panel.urls).map(Files.outgoing)) }
        focusRequest += 1
    }

    private static func plain(_ s: String) -> NSAttributedString {
        NSAttributedString(string: s, attributes: ComposerText.display([]))
    }

    private func submit() {
        let outgoing = text
        let message = ComposerText.message(from: outgoing)
        guard Composer.canSend(message.body) || !store.outgoingFiles.isEmpty else { return }
        text = NSAttributedString()
        Task {
            // give the text back if the core couldn't take it
            if await !store.send(message.body, ranges: message.ranges), text.length == 0 { text = outgoing }
        }
    }
}

/// Composer decisions, kept pure for tests.
enum Composer {
    enum ReturnAction: Equatable { case send, newline }

    /// ↩ sends, ⇧↩ (or ⌥↩) inserts a newline.
    static func action(for modifiers: NSEvent.ModifierFlags) -> ReturnAction {
        modifiers.contains(.shift) || modifiers.contains(.option) ? .newline : .send
    }

    /// An input method (Japanese, Chinese, Korean…) is composing in the focused text view.
    @MainActor static func isComposing() -> Bool {
        (NSApp.keyWindow?.firstResponder as? NSTextView)?.hasMarkedText() ?? false
    }

    struct PasteError: LocalizedError {
        var errorDescription: String? { "the image couldn't be read" }
    }

    /// What ⌘V attaches: copied files, else a copied image without text (a screenshot, saved as
    /// a PNG). Empty for text, which the field pastes itself, even when it also carries a picture
    /// of itself (Pages, Word, Numbers do that).
    static func pastedFiles(from board: NSPasteboard) throws -> [URL] {
        let files = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]
        if let files, !files.isEmpty { return Files.attachable(files) }
        guard board.availableType(from: [.string]) == nil, NSImage.canInit(with: board) else { return [] }
        guard let image = NSImage(pasteboard: board), let tiff = image.tiffRepresentation,
            let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:])
        else { throw PasteError() }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("Semaphore-paste", isDirectory: true)
        let url = dir.appendingPathComponent("Pasted image \(UUID().uuidString.prefix(8)).png")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try png.write(to: url)
        return [url]
    }

    static func canSend(_ text: String) -> Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    nonisolated(unsafe) static var defaults: UserDefaults = .standard  // swapped only by tests

    static func draft(for conversationId: String) -> String {
        defaults.string(forKey: "draft.\(conversationId)") ?? ""
    }

    static func saveDraft(_ text: String, for conversationId: String) {
        if canSend(text) {
            defaults.set(text, forKey: "draft.\(conversationId)")
        } else {
            defaults.removeObject(forKey: "draft.\(conversationId)")
        }
    }
}

/// The members matching the "@word" being typed; ↑/↓ choose, ↩ or a click picks.
struct MentionList: View {
    let picker: MentionPicker

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(picker.matches.prefix(8).enumerated()), id: \.element.0) { i, member in
                Button {
                    picker.pick(i)
                } label: {
                    HStack(spacing: 8) {
                        Avatar(id: "c:\(member.0)", name: member.1, size: 22)
                        Text(member.1).font(.system(size: 13))
                        Spacer()
                    }
                    .padding(.horizontal, 10).padding(.vertical, 5)
                    .background(i == picker.selected ? Theme.outgoing.opacity(0.35) : .clear)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.vertical, 4)
        .background(Theme.field, in: RoundedRectangle(cornerRadius: 12))
        .padding(.horizontal, 14)
        .accessibilityLabel("Mention someone")
    }
}

/// Files waiting to go with the next message, each removable.
struct FileTray: View {
    let store: AppStore

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 8) {
                ForEach(store.outgoingFiles, id: \.path) { f in
                    HStack(spacing: 6) {
                        if f.contentType.hasPrefix("image/") {
                            ThumbnailImage(path: f.path).frame(width: 36, height: 36)
                                .clipShape(RoundedRectangle(cornerRadius: 6))
                        } else {
                            Image(systemName: "doc.fill").font(.system(size: 20)).frame(width: 36, height: 36)
                        }
                        Text(URL(fileURLWithPath: f.path).lastPathComponent).font(.system(size: 12)).lineLimit(1)
                            .frame(maxWidth: 140)
                        Button {
                            store.removeFile(f.path)
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                        }
                        .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                        .accessibilityLabel("Remove \(URL(fileURLWithPath: f.path).lastPathComponent)")
                    }
                    .padding(6)
                    .background(Theme.field, in: RoundedRectangle(cornerRadius: 10))
                }
            }
            .padding(.horizontal, 14)
        }
        .scrollIndicators(.never)
    }
}
