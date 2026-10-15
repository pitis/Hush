import AppKit
import SwiftUI

struct ComposerView: View {
    let store: AppStore
    let conversationId: String
    @State private var text = ""
    @FocusState private var focused: Bool
    /// ⌘V of files or images while the field is focused (the field itself only pastes text).
    @State private var pasteMonitor: Any?

    var body: some View {
        VStack(spacing: 6) {
            if let reply = store.replyTo {
                HStack {
                    Rectangle().fill(Theme.outgoing).frame(width: 3)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(
                            reply.isOutgoing ? "Replying to yourself" : "Replying to \(store.senderName(reply.author))"
                        )
                        .font(.system(size: 11, weight: .semibold))
                        Text(reply.body ?? "Attachment").font(.system(size: 12)).lineLimit(1)
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
            if !store.outgoingFiles.isEmpty { FileTray(store: store) }
            HStack(alignment: .bottom, spacing: 10) {
                Button(action: chooseFiles) {
                    Image(systemName: "paperclip").font(.system(size: 18))
                }
                .accessibilityLabel("Attach files")
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .padding(.bottom, 9)
                Button {
                    focused = true  // the palette types into the focused field
                    NSApp.orderFrontCharacterPalette(nil)
                } label: {
                    Image(systemName: "face.smiling").font(.system(size: 20))
                }
                .accessibilityLabel("Emoji")
                .buttonStyle(.plain).foregroundStyle(Theme.secondaryText)
                .padding(.bottom, 8)
                TextField("Message", text: $text, axis: .vertical)
                    .textFieldStyle(.plain)
                    .font(.system(size: 14))
                    .lineLimit(1...6)
                    .focused($focused)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 9)
                    .background(Theme.field, in: RoundedRectangle(cornerRadius: 18))
                    .onKeyPress(.return, phases: .down) { press in
                        switch Composer.action(for: press.modifiers) {
                        case .send:
                            submit()
                            return .handled
                        case .newline:
                            text += "\n"
                            return .handled
                        }
                    }
            }
            .padding(.horizontal, 14)
            .padding(.bottom, 12)
            .padding(.top, store.replyTo == nil ? 8 : 0)
        }
        .onAppear {
            text = Composer.draft(for: conversationId)
            takeFocusIfAsked()
        }
        .onChange(of: conversationId) { _, new in text = Composer.draft(for: new) }
        .onChange(of: text) { _, new in Composer.saveDraft(new, for: conversationId) }
        .onAppear { pasteMonitor = installPasteMonitor() }
        .onDisappear {
            if let pasteMonitor { NSEvent.removeMonitor(pasteMonitor) }
            pasteMonitor = nil
        }
        .onChange(of: store.composerWantsFocus) { _, _ in takeFocusIfAsked() }
    }

    private func takeFocusIfAsked() {
        guard store.composerWantsFocus else { return }
        focused = true
        store.composerTookFocus()
    }

    private func chooseFiles() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        if panel.runModal() == .OK { store.addFiles(Files.attachable(panel.urls).map(Files.outgoing)) }
        focused = true
    }

    /// Menu shortcuts (⌘V is Edit ▸ Paste) reach the text field before SwiftUI's key handlers, so
    /// a local monitor sees ⌘V first and takes it only when files or an image are being pasted.
    private func installPasteMonitor() -> Any? {
        NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            guard event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
                event.charactersIgnoringModifiers == "v",
                MainActor.assumeIsolated({ focused && event.window === NSApp.mainWindow })
            else { return event }
            do {
                let files = try Composer.pastedFiles(from: .general)
                guard !files.isEmpty else { return event }
                MainActor.assumeIsolated { store.addFiles(files.map(Files.outgoing)) }
            } catch {
                MainActor.assumeIsolated {
                    store.showError("Couldn't attach what you pasted: \(error.localizedDescription)")
                }
            }
            return nil
        }
    }

    private func submit() {
        let outgoing = text
        guard Composer.canSend(outgoing) || !store.outgoingFiles.isEmpty else { return }
        text = ""
        Task {
            // give the text back if the core couldn't take it
            if await !store.send(outgoing), text.isEmpty { text = outgoing }
        }
    }
}

/// Composer decisions, kept pure for tests.
enum Composer {
    enum ReturnAction: Equatable { case send, newline }

    /// ↩ sends, ⇧↩ (or ⌥↩) inserts a newline.
    static func action(for modifiers: EventModifiers) -> ReturnAction {
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
