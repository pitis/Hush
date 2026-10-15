import SwiftUI

struct ComposerView: View {
    let store: AppStore
    let conversationId: String
    @State private var text = ""
    @FocusState private var focused: Bool

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
            HStack(alignment: .bottom, spacing: 10) {
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
            focused = true
        }
        .onChange(of: conversationId) { _, new in
            text = Composer.draft(for: new)
            focused = true
        }
        .onChange(of: text) { _, new in Composer.saveDraft(new, for: conversationId) }
    }

    private func submit() {
        let outgoing = text
        guard Composer.canSend(outgoing) else { return }
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
