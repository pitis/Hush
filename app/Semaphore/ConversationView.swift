import QuickLook
import SwiftUI

struct ConversationView: View {
    let store: AppStore
    let conversation: Conversation
    @State private var quickLook: URL?
    @State private var dropTargeted = false

    var body: some View {
        VStack(spacing: 0) {
            ConversationHeader(store: store, conversation: conversation)
            Divider().overlay(Color.black.opacity(0.4))
            TimelineList(store: store, conversation: conversation) { a in
                do { quickLook = try Files.openable(a) } catch {
                    store.showError("Couldn't open \(a.fileName ?? "the file"): \(error.localizedDescription)")
                }
            }
            ComposerView(store: store, conversationId: conversation.id)
        }
        .background(Theme.window)
        .quickLookPreview($quickLook)
        // the preview closed: its link goes
        .onChange(of: quickLook) { old, _ in if let old { Files.close(old) } }
        .onDisappear { if let quickLook { Files.close(quickLook) } }
        // files dropped anywhere on the chat wait in the composer's tray
        .dropDestination(for: URL.self) { urls, _ in
            let files = Files.attachable(urls).map(Files.outgoing)
            store.addFiles(files)
            return !files.isEmpty
        } isTargeted: {
            dropTargeted = $0
        }
        .overlay {
            if dropTargeted {
                RoundedRectangle(cornerRadius: 12).stroke(Theme.outgoing, lineWidth: 3).padding(4)
                    .allowsHitTesting(false)
            }
        }
    }
}

/// Header in its own view so typing/connection changes don't re-render the timeline.
struct ConversationHeader: View {
    let store: AppStore
    let conversation: Conversation

    var body: some View {
        let title = store.title(conversation)
        HStack(spacing: 12) {
            Avatar(id: conversation.id, name: title, size: 36)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 15, weight: .semibold)).lineLimit(1)
                if store.isTyping(conversation.id) {
                    Text("typing…").font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                } else if store.connection != .online {
                    Text(String(describing: store.connection)).font(.system(size: 12))
                        .foregroundStyle(Theme.secondaryText)
                }
            }
            Spacer()
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 10)
    }
}

struct TimelineList: View {
    let store: AppStore
    let conversation: Conversation
    let open: (Attachment) -> Void
    @State private var atBottom = true
    @State private var nearTop = false
    @State private var unseenNew = false
    @Environment(\.controlActiveState) private var activeState

    var body: some View {
        let isGroup = conversation.kind == .group
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 0) {
                    if store.hasOlder { ProgressView().controlSize(.small).padding(8) }
                    ForEach(store.timelineItems) { item in
                        switch item {
                        case .day(let title, _):
                            Text(title).font(.system(size: 12, weight: .medium)).foregroundStyle(Theme.secondaryText)
                                .padding(.vertical, 10)
                        case .message(let m, let first, let last):
                            let key = MessageKey(author: m.author, sentAt: m.sentAt)
                            MessageRow(
                                message: m, first: first, last: last, isGroup: isGroup,
                                senderName: store.senderName(m.author),
                                reactions: store.reactions[key] ?? [],
                                files: store.attachments[key] ?? [], preview: store.previews[key],
                                reply: { store.replyTo = m },
                                retry: { Task { await store.retry(key) } },
                                download: store.download, open: open)
                        }
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            .defaultScrollAnchor(.bottom)
            .onScrollGeometryChange(for: [Bool].self) { g in
                [
                    Timeline.isAtBottom(
                        offsetY: g.contentOffset.y, visibleHeight: g.containerSize.height,
                        contentHeight: g.contentSize.height),
                    Timeline.isNearTop(offsetY: g.contentOffset.y),
                ]
            } action: { _, edges in
                atBottom = edges[0]
                if atBottom { unseenNew = false }
                nearTop = edges[1]
            }
            .onChange(of: nearTop) { _, near in if near { loadOlder() } }
            .onChange(of: store.messages.first?.sentAt) { _, _ in
                // still at the top after a page arrived (short messages): keep paging
                if nearTop { loadOlder() }
            }
            .onChange(of: store.messages.last?.sentAt) { _, _ in
                let own = store.messages.last?.isOutgoing ?? false
                switch Timeline.onNewLastMessage(atBottom: atBottom, isOwn: own) {
                case .scrollToBottom:
                    withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("bottom", anchor: .bottom) }
                    unseenNew = false
                case .showPill: unseenNew = true
                case .nothing: break
                }
            }
            .onChange(of: conversation.id) { _, _ in
                unseenNew = false
                proxy.scrollTo("bottom", anchor: .bottom)
            }
            // read once the newest message is on screen in the key window, like Desktop
            .onChange(of: atBottom) { _, _ in markReadIfSeen() }
            .onChange(of: activeState) { _, _ in markReadIfSeen() }
            .onChange(of: conversation.unreadCount) { _, _ in markReadIfSeen() }
            .onChange(of: conversation.id) { _, _ in markReadIfSeen() }
            .onReceive(NotificationCenter.default.publisher(for: NSWindow.didChangeOcclusionStateNotification)) {
                _ in markReadIfSeen()
            }
            .onAppear { markReadIfSeen() }
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

    private func markReadIfSeen() {
        let visible = NSApp.keyWindow?.occlusionState.contains(.visible) ?? false
        if ChatList.shouldMarkRead(
            atBottom: atBottom, windowKey: activeState == .key, windowVisible: visible,
            unread: conversation.unreadCount)
        {
            store.markRead(conversation.id)
        }
    }

    private func loadOlder() {
        guard store.hasOlder else { return }
        Task { await store.loadOlder() }
    }
}

struct MessageRow: View {
    let message: Message
    let first: Bool
    let last: Bool
    let isGroup: Bool
    let senderName: String
    let reactions: [Reaction]
    let files: [Attachment]
    let preview: LinkPreview?
    let reply: () -> Void
    let retry: () -> Void
    let download: (Int64) -> Void
    let open: (Attachment) -> Void

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
            if !message.deleted { Button("Reply", action: reply) }
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
        // files without a caption: the time sits under them on the right, the bubble as wide as they are
        let filesOnly = !files.isEmpty && (message.body ?? "").isEmpty && !message.deleted
        return VStack(alignment: filesOnly ? .trailing : .leading, spacing: 4) {
            if let q = message.quote, !message.deleted {
                VStack(alignment: .leading, spacing: 1) {
                    Text(q.author == message.author ? "Reply" : "Quoted").font(.system(size: 11, weight: .semibold))
                    Text(q.text ?? "Attachment").font(.system(size: 12)).lineLimit(2)
                }
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.black.opacity(0.18), in: RoundedRectangle(cornerRadius: 10))
            }
            if !message.deleted {
                AttachmentList(files: files, download: download, open: open)
                if let preview { LinkPreviewCard(preview: preview) }
            }
            HStack(alignment: .lastTextBaseline, spacing: 8) {
                Group {
                    if message.deleted {
                        Text("This message was deleted").italic().foregroundStyle(.white.opacity(0.7))
                    } else if let body = message.body, !body.isEmpty {
                        Text(body).textSelection(.enabled)
                    } else if files.isEmpty {
                        Text("Attachment")
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
    }

    private var meta: some View {
        HStack(spacing: 3) {
            if message.edited && !message.deleted { Text("edited") }
            Text(Formatting().bubbleTime(message.sentAt))
            if outgoing {
                tick.accessibilityLabel(Timeline.accessibilityLabel(for: Timeline.tick(for: message.status)))
            }
        }
        .font(.system(size: 10))
        .foregroundStyle(.white.opacity(0.7))
        .fixedSize()
    }

    @ViewBuilder private var tick: some View {
        switch Timeline.tick(for: message.status) {
        case .failed:
            Button(action: retry) {
                Label("Retry", systemImage: "exclamationmark.triangle.fill").labelStyle(.titleAndIcon)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.yellow)
            .accessibilityLabel("Not sent. Retry")
        case let other: TickIcon(tick: other)
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

/// A message's delivery state, in bubbles and in the chat list.
struct TickIcon: View {
    let tick: Timeline.Tick

    var body: some View {
        Group {
            switch tick {
            case .clock: Image(systemName: "clock")
            case .single: Image(systemName: "checkmark")
            case .double: Image(systemName: "checkmark.circle")
            case .doubleFilled: Image(systemName: "checkmark.circle.fill")
            case .failed: Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow)
            }
        }
        .accessibilityLabel(Timeline.accessibilityLabel(for: tick))
    }
}
