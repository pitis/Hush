import SwiftUI

struct ConversationView: View {
    let store: AppStore
    let conversation: Conversation
    @State private var atBottom = true
    @State private var unseenNew = false

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider().overlay(Color.black.opacity(0.4))
            timeline
            ComposerView(store: store, conversationId: conversation.id)
        }
        .background(Theme.window)
    }

    private var header: some View {
        let title = AppStore.displayTitle(conversation)
        return HStack(spacing: 12) {
            Avatar(id: conversation.id, name: title, size: 36)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 15, weight: .semibold)).lineLimit(1)
                if store.isTyping(conversation.id) {
                    Text("typing…").font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                } else if store.connection != .online {
                    Text(String(describing: store.connection)).font(.system(size: 12)).foregroundStyle(
                        Theme.secondaryText)
                }
            }
            Spacer()
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 10)
    }

    private var timeline: some View {
        let isGroup = conversation.kind == .group
        return ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 0) {
                    if store.hasOlder {
                        ProgressView().controlSize(.small).padding(8)
                            .onAppear { Task { await store.loadOlder() } }
                    }
                    ForEach(Timeline.items(for: store.messages)) { item in
                        switch item {
                        case .day(let title, _):
                            Text(title).font(.system(size: 12, weight: .medium)).foregroundStyle(Theme.secondaryText)
                                .padding(.vertical, 10)
                        case .message(let m, let first, let last):
                            MessageRow(
                                message: m, first: first, last: last, isGroup: isGroup,
                                senderName: store.senderName(m.author),
                                reactions: store.reactions[MessageKey(author: m.author, sentAt: m.sentAt)] ?? [],
                                reply: { store.replyTo = m },
                                retry: { Task { await store.retry(MessageKey(author: m.author, sentAt: m.sentAt)) } })
                        }
                    }
                    Color.clear.frame(height: 1).id("bottom")
                        .onAppear {
                            atBottom = true
                            unseenNew = false
                        }
                        .onDisappear { atBottom = false }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            .defaultScrollAnchor(.bottom)
            .onChange(of: store.messages.last?.sentAt) { _, _ in
                // follow new messages only when the user is already at the bottom
                if atBottom {
                    withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("bottom", anchor: .bottom) }
                } else {
                    unseenNew = true
                }
            }
            .onChange(of: conversation.id) { _, _ in proxy.scrollTo("bottom", anchor: .bottom) }
            .overlay(alignment: .bottomTrailing) {
                if unseenNew {
                    Button {
                        proxy.scrollTo("bottom", anchor: .bottom)
                        unseenNew = false
                    } label: {
                        Label("New messages", systemImage: "arrow.down").font(.system(size: 12, weight: .semibold))
                            .padding(.horizontal, 12).padding(.vertical, 7)
                            .background(Theme.outgoing, in: Capsule()).foregroundStyle(.white)
                    }
                    .buttonStyle(.plain)
                    .padding(16)
                }
            }
        }
    }
}

struct MessageRow: View {
    let message: Message
    let first: Bool
    let last: Bool
    let isGroup: Bool
    let senderName: String
    let reactions: [Reaction]
    let reply: () -> Void
    let retry: () -> Void

    private var outgoing: Bool { message.isOutgoing }

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            if outgoing { Spacer(minLength: 80) }
            if isGroup && !outgoing {
                if last {
                    Avatar(id: "c:\(message.author)", name: senderName, size: 28)
                } else {
                    Color.clear.frame(width: 28, height: 28)
                }
            }
            VStack(alignment: outgoing ? .trailing : .leading, spacing: 2) {
                if isGroup && !outgoing && first {
                    Text(senderName).font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.avatars[Formatting.avatarIndex("c:\(message.author)")])
                        .padding(.horizontal, 12)
                }
                bubble
                if !reactions.isEmpty { reactionPill }
            }
            .frame(maxWidth: 560, alignment: outgoing ? .trailing : .leading)
            if !outgoing { Spacer(minLength: 80) }
        }
        .padding(.top, first ? 8 : 2)
        .contextMenu {
            Button("Reply", action: reply)
            if let body = message.body, !message.deleted {
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(body, forType: .string)
                }
            }
            if message.status == .failed { Button("Retry", action: retry) }
        }
    }

    private var bubble: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let q = message.quote, !message.deleted {
                VStack(alignment: .leading, spacing: 1) {
                    Text(q.author == message.author ? "Reply" : "Quoted").font(.system(size: 11, weight: .semibold))
                    Text(q.text ?? "Attachment").font(.system(size: 12)).lineLimit(2)
                }
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.black.opacity(0.18), in: RoundedRectangle(cornerRadius: 10))
            }
            HStack(alignment: .lastTextBaseline, spacing: 8) {
                Group {
                    if message.deleted {
                        Text("This message was deleted").italic().foregroundStyle(.white.opacity(0.7))
                    } else {
                        Text(message.body ?? "Attachment").textSelection(.enabled)
                    }
                }
                .font(.system(size: 14))
                meta
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .foregroundStyle(.white)
        .background(outgoing ? Theme.outgoing : Theme.incoming, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
        .onTapGesture(count: 1) { if message.status == .failed { retry() } }
    }

    private var meta: some View {
        HStack(spacing: 3) {
            if message.edited && !message.deleted { Text("edited") }
            Text(Formatting().bubbleTime(message.sentAt))
            if outgoing { tick }
        }
        .font(.system(size: 10))
        .foregroundStyle(.white.opacity(0.7))
        .fixedSize()
    }

    @ViewBuilder private var tick: some View {
        switch Timeline.tick(for: message.status) {
        case .clock: Image(systemName: "clock")
        case .single: Image(systemName: "checkmark")
        case .double: Image(systemName: "checkmark.circle")
        case .doubleFilled: Image(systemName: "checkmark.circle.fill")
        case .failed:
            Label("Retry", systemImage: "exclamationmark.triangle.fill").foregroundStyle(.yellow)
                .labelStyle(.titleAndIcon)
        }
    }

    private var reactionPill: some View {
        let counts = Dictionary(grouping: reactions, by: \.emoji).map { ($0.key, $0.value.count) }
            .sorted { $0.1 > $1.1 }
        return HStack(spacing: 4) {
            ForEach(counts, id: \.0) { emoji, count in
                Text(count > 1 ? "\(emoji) \(count)" : emoji).font(.system(size: 12))
            }
        }
        .padding(.horizontal, 8).padding(.vertical, 3)
        .background(Theme.field, in: Capsule())
        .overlay(Capsule().stroke(Theme.window, lineWidth: 2))
        .offset(y: -6)
    }
}
