import AppKit
import SwiftUI

struct ComposerView: View {
    let store: AppStore
    let conversationId: String
    @State private var text = NSAttributedString()
    /// Bumped to put the caret in the field.
    @State private var focusRequest = 0
    @State private var showStickers = false
    /// What was typed before editing started; back once the edit is saved or cancelled.
    @State private var draftBeforeEdit: NSAttributedString?
    @State private var mentions = MentionPicker()
    /// Waits for a pause in typing before asking for a link preview.
    @State private var previewWait: Task<Void, Never>?

    /// Something shows above the field: picker, link preview, reply, edit or files.
    private var hasAccessories: Bool {
        mentions.isOpen || store.previewDraft != nil || store.replyTo != nil || store.editing != nil
            || !store.outgoingFiles.isEmpty
    }

    var body: some View {
        // glass for the floating controls only; the timeline scrolls on under them
        GlassEffectContainer(spacing: 8) {
            VStack(spacing: 8) {
                if hasAccessories {
                    accessories
                        .padding(8)
                        .glassEffect(.regular, in: .rect(cornerRadius: 16, style: .continuous))
                }
                HStack(alignment: .bottom, spacing: 8) {
                    addMenu
                    ComposerField(
                        text: $text, focusRequest: focusRequest, onReturn: submit,
                        // ↑ in an empty field edits your last message, as in Desktop
                        onUpInEmpty: { store.editing == nil && store.editLast() },
                        onPasteFiles: { store.addFiles($0.map(Files.outgoing)) },
                        onPasteError: {
                            store.showError("Couldn't attach what you pasted: \($0.localizedDescription)")
                        },
                        mentions: conversationId.hasPrefix("g:") ? mentions : nil
                    )
                    .padding(.horizontal, 14)
                    .padding(.vertical, Self.fieldInset)
                    .glassEffect(.regular, in: .rect(cornerRadius: 18, style: .continuous))
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.top, 8)
        .padding(.bottom, 12)
        .onChange(of: store.attachRequest) { _, _ in chooseFiles() }
        .onAppear {
            text = Self.plain(Composer.draft(for: conversationId))
            takeFocusIfAsked()
        }
        .task(id: conversationId) {
            mentions.reset()
            mentions.members = await store.mentionCandidates(conversationId)
        }
        .onChange(of: conversationId) { _, new in
            // an edit doesn't follow to another chat, nor does the draft it put aside
            draftBeforeEdit = nil
            text = Self.plain(Composer.draft(for: new))
        }
        .onChange(of: text) { _, new in
            // drafts keep the words, not their styles
            if store.editing == nil { Composer.saveDraft(new.string, for: conversationId) }
            previewWait?.cancel()
            previewWait = Task {
                try? await Task.sleep(for: .milliseconds(700))
                guard !Task.isCancelled else { return }
                await store.composerChanged(new.string)
            }
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

    /// Above and below the field's text.
    static let fieldInset: CGFloat = 9
    /// The field's height with one line (as `ComposerField` measures it): the + is as tall, so it
    /// sits centred on one line and stays by the last as the field grows.
    static let oneLine =
        NSLayoutManager().defaultLineHeight(for: .systemFont(ofSize: ComposerText.size)).rounded(.up) + 2 * fieldInset

    /// Attach Files… and Stickers…; emoji are under Edit ▸ Emoji & Symbols (⌃⌘Space).
    private var addMenu: some View {
        Menu {
            Button("Attach Files…", action: chooseFiles)
            Button("Stickers…") { showStickers = true }
        } label: {
            Image(systemName: "plus").font(.system(size: 15, weight: .medium))
                .frame(width: Self.oneLine, height: Self.oneLine)
                .contentShape(Circle())
        }
        .menuStyle(.button)
        .menuIndicator(.hidden)
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: Circle())
        .disabled(store.editing != nil)
        .help("Attach files or a sticker")
        .accessibilityLabel("Attach")
        .popover(isPresented: $showStickers, arrowEdge: .top) {
            StickerPicker(store: store) { showStickers = false }
        }
    }

    /// What goes with the next message, above the field.
    @ViewBuilder private var accessories: some View {
        VStack(alignment: .leading, spacing: 8) {
            if mentions.isOpen { MentionList(picker: mentions) }
            if let draft = store.previewDraft { PreviewDraftCard(draft: draft, dismiss: store.dismissPreview) }
            if let reply = store.replyTo {
                AccessoryBar(
                    icon: "arrowshape.turn.up.left",
                    title: reply.isOutgoing ? "Replying to yourself" : "Replying to \(store.senderName(reply.author))",
                    detail: reply.body.map { MessageText.plain($0, ranges: reply.ranges, name: store.mentionName) }
                        ?? "Attachment",
                    close: "Cancel reply", cancelsWithEsc: !mentions.isOpen
                ) { store.replyTo = nil }
            }
            if let editing = store.editing {
                AccessoryBar(
                    icon: "pencil", title: "Editing message",
                    detail: editing.body.map {
                        MessageText.plain($0, ranges: editing.ranges, name: store.mentionName)
                    } ?? "",
                    close: "Cancel editing", cancelsWithEsc: !mentions.isOpen, action: store.cancelEditing)
            }
            if !store.outgoingFiles.isEmpty { FileTray(store: store) }
        }
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
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("Hush-paste", isDirectory: true)
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

/// A reply or an edit in progress, above the field, with ✕ (or Esc) to drop it.
struct AccessoryBar: View {
    let icon: String
    let title: String
    let detail: String
    /// What ✕ is called, for VoiceOver.
    let close: String
    /// Esc drops it (not while the mention picker has Esc).
    let cancelsWithEsc: Bool
    let action: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: icon).foregroundStyle(.secondary).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 11, weight: .semibold))
                Text(detail).font(.system(size: 12)).lineLimit(1).foregroundStyle(Theme.secondaryText)
            }
            Spacer()
            Button(action: action) { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .keyboardShortcut(cancelsWithEsc ? .cancelAction : nil)
                .accessibilityLabel(close)
        }
        .padding(.horizontal, 4)
    }
}

/// The link preview that will go with the message, with ✕ to leave it off.
struct PreviewDraftCard: View {
    let draft: PreviewDraft
    let dismiss: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            if let image = draft.image {
                ThumbnailImage(path: image.path).frame(width: 44, height: 44)
                    .clipShape(RoundedRectangle(cornerRadius: 6))
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(draft.title ?? URL(string: draft.url)?.host() ?? draft.url)
                    .font(.system(size: 12, weight: .semibold)).lineLimit(1)
                if let description = draft.description {
                    Text(description).font(.system(size: 11)).lineLimit(2).foregroundStyle(Theme.secondaryText)
                }
                Text(URL(string: draft.url)?.host() ?? draft.url).font(.system(size: 11))
                    .foregroundStyle(Theme.secondaryText).lineLimit(1)
            }
            Spacer()
            Button(action: dismiss) { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .accessibilityLabel("Remove the link preview")
        }
    }
}

/// The members matching the "@word" being typed; ↑/↓ choose, ↩ or a click picks.
struct MentionList: View {
    let picker: MentionPicker

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(picker.matches.enumerated()), id: \.element.0) { i, member in
                Button {
                    picker.pick(i)
                } label: {
                    HStack(spacing: 8) {
                        Avatar(id: "c:\(member.0)", name: member.1, size: 22)
                        Text(member.1).font(.system(size: 13))
                        Spacer()
                    }
                    .padding(.horizontal, 10).padding(.vertical, 5)
                    .background(
                        i == picker.selected ? Color.accentColor.opacity(0.3) : .clear,
                        in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                    )
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
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
        }
        .scrollIndicators(.never)
    }
}
