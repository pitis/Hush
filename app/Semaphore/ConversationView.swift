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
            if conversation.blocked || conversation.messageRequest {
                RequestBar(store: store, conversation: conversation)
            } else {
                ComposerView(store: store, conversationId: conversation.id)
            }
        }
        .background(Theme.window)
        .quickLookPreview($quickLook)
        // the preview closed: its link goes
        .onChange(of: quickLook) { old, _ in if let old { Files.close(old) } }
        .onDisappear { if let quickLook { Files.close(quickLook) } }
        // files dropped anywhere on the chat wait in the composer's tray
        .dropDestination(for: URL.self) { urls, _ in
            guard !conversation.blocked, !conversation.messageRequest else { return false }
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

/// In place of the composer, as Desktop's: a message request's Accept, Delete and Block, or a
/// blocked chat's Unblock and Delete. Block and Delete ask first.
struct RequestBar: View {
    let store: AppStore
    let conversation: Conversation
    @State private var confirming: Confirm?

    private enum Confirm: Identifiable {
        case block, delete
        var id: Self { self }
    }

    private var isGroup: Bool { conversation.kind == .group }

    var body: some View {
        let name = store.title(conversation)
        VStack(spacing: 10) {
            Text(RequestBar.prompt(conversation, name: name))
                .font(.system(size: 13)).multilineTextAlignment(.center)
                .foregroundStyle(Theme.secondaryText)
            HStack(spacing: 10) {
                if conversation.blocked {
                    Button("Delete") { confirming = .delete }
                    Button("Unblock") { store.acceptRequest(conversation.id) }
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Block") { confirming = .block }
                    Button("Delete") { confirming = .delete }
                    Button("Accept") { store.acceptRequest(conversation.id) }
                        .keyboardShortcut(.defaultAction)
                }
            }
            .controlSize(.large)
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 18).padding(.vertical, 14)
        .confirmationDialog(
            confirming == .block ? RequestBar.blockTitle(conversation, name: name) : "Delete this chat?",
            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
            presenting: confirming
        ) { c in
            switch c {
            case .block: Button("Block", role: .destructive) { store.block(conversation.id) }
            case .delete: Button("Delete", role: .destructive) { store.deleteChat(conversation.id) }
            }
        } message: { c in
            switch c {
            case .block: Text(RequestBar.blockMessage(conversation))
            case .delete:
                Text(
                    isGroup
                        ? "Its messages are deleted from this Mac."
                        : "Its messages are deleted from this Mac and your phone.")
            }
        }
    }

    /// What the bar says above its buttons.
    nonisolated static func prompt(_ c: Conversation, name: String) -> String {
        if c.blocked {
            return c.kind == .group ? "You blocked this group." : "You blocked \(name)."
        }
        return c.kind == .group
            ? "Join this group? They won't know you've seen their messages until you accept."
            : "Let \(name) message you? They won't know you've seen their messages until you accept."
    }

    nonisolated static func blockTitle(_ c: Conversation, name: String) -> String {
        c.kind == .group ? "Block this group?" : "Block \(name)?"
    }

    nonisolated static func blockMessage(_ c: Conversation) -> String {
        c.kind == .group
            ? "You won't get its messages or updates. You stay a member: leave it from your phone."
            : "They won't be able to message or call you."
    }
}

/// Header in its own view so typing/connection changes don't re-render the timeline.
struct ConversationHeader: View {
    let store: AppStore
    let conversation: Conversation

    private var canCall: Bool {
        conversation.kind == .direct && !conversation.blocked && !conversation.messageRequest
            && !conversation.id.hasPrefix("c:PNI:")
            && conversation.id != store.ownAci.map { "c:\($0)" }
    }

    var body: some View {
        let title = store.title(conversation)
        HStack(spacing: 12) {
            Avatar(id: conversation.id, name: title, size: 36)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 15, weight: .semibold)).lineLimit(1)
                if conversation.expireTimer > 0 {
                    Label(Placeholder.duration(Int64(conversation.expireTimer)), systemImage: "hourglass")
                        .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                        .accessibilityLabel(
                            "Disappearing messages: \(Placeholder.duration(Int64(conversation.expireTimer)))")
                }
                if store.isTyping(conversation.id) {
                    Text("typing…").font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                } else if store.connection != .online {
                    Text(String(describing: store.connection)).font(.system(size: 12))
                        .foregroundStyle(Theme.secondaryText)
                }
            }
            Spacer()
            if canCall {
                let inCall = store.call != nil && store.call?.phase != .ended
                Button {
                    if inCall { store.showCallWindow() } else { store.startCall(conversation.id) }
                } label: {
                    Image(systemName: inCall ? "phone.fill" : "phone").font(.system(size: 15))
                }
                .buttonStyle(.bordered)
                // signaling rides the message pipeline: no call before it is connected
                .disabled(!inCall && store.connection != .online)
                .help(inCall ? "Show the call" : "Voice call")
                .accessibilityLabel(inCall ? "Show the call" : "Start a voice call")
            }
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
                        case .event(let e):
                            ChatEventRow(
                                event: e, name: store.senderName(e.author), me: store.ownAci,
                                reset: { store.resetSession(e.conversationId) })
                        case .message(let m, let first, let last):
                            let key = MessageKey(author: m.author, sentAt: m.sentAt)
                            MessageRow(
                                message: m, first: first, last: last, isGroup: isGroup,
                                senderName: store.senderName(m.author), mentionName: store.mentionName,
                                reactions: store.reactions[key] ?? [],
                                files: store.attachments[key] ?? [], preview: store.previews[key],
                                highlighted: store.highlighted == key,
                                reply: { store.reply(to: m) },
                                retry: { Task { await store.retry(key) } },
                                download: store.download, open: open,
                                actions: MessageRow.Actions(
                                    me: store.ownAci, mine: store.myReaction(to: m),
                                    canEdit: MessageActions.canEdit(m, me: store.ownAci),
                                    canDelete: MessageActions.canDelete(m, me: store.ownAci),
                                    react: { emoji in Task { await store.react(to: m, with: emoji) } },
                                    edit: { store.startEditing(m) },
                                    delete: { Task { await store.deleteForEveryone(m) } },
                                    name: store.senderName))
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
            // a search result opened: to the message, in the middle
            .onChange(of: store.scrollRequest) { _, _ in
                guard let k = store.scrollTarget else { return }
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("\(k.author)-\(k.sentAt)", anchor: .center) }
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
            unread: ChatList.isUnread(conversation))
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
    /// A mentioned person's name.
    var mentionName: (String) -> String = { String($0.prefix(8)) }
    let reactions: [Reaction]
    let files: [Attachment]
    let preview: LinkPreview?
    /// Just opened from search: outlined for a moment.
    var highlighted = false
    let reply: () -> Void
    let retry: () -> Void
    let download: (Int64) -> Void
    let open: (Attachment) -> Void
    var actions = Actions()
    @State private var hovering = false
    @State private var picking = false
    @State private var showingReactors = false
    @State private var confirmingDelete = false
    @State private var spoilersRevealed = false

    /// React, edit, delete (after Signal Desktop).
    struct Actions {
        var me: String?
        /// Our reaction, counting one just sent.
        var mine: String?
        var canEdit = false
        var canDelete = false
        var react: (String) -> Void = { _ in }
        var edit: () -> Void = {}
        var delete: () -> Void = {}
        /// A reactor's display name.
        var name: (String) -> String = { $0 }
    }

    private var outgoing: Bool { message.isOutgoing }
    private var mine: String? { actions.mine }

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
                HStack(alignment: .center, spacing: 6) {
                    if outgoing { reactButton }
                    bubble
                    if !outgoing { reactButton }
                }
                if !reactions.isEmpty { reactionPill }
            }
            .frame(maxWidth: 560, alignment: outgoing ? .trailing : .leading)
            if !outgoing { Spacer(minLength: 80) }
        }
        .padding(.top, first ? 8 : 2)
        .onHover { hovering = $0 }
        .contextMenu {
            if !message.deleted {
                Menu("React") {
                    ForEach(MessageActions.quickReactions, id: \.self) { emoji in
                        Button(emoji == mine ? "\(emoji) ✓" : emoji) { actions.react(emoji) }
                    }
                }
                Button("Reply", action: reply)
            }
            if actions.canEdit { Button("Edit", action: actions.edit) }
            if let body = message.body, !message.deleted {
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    let text = MessageText.plain(
                        body, ranges: message.ranges, name: mentionName, revealSpoilers: true)
                    NSPasteboard.general.setString(text, forType: .string)
                }
            }
            if message.status == .failed { Button("Retry", action: retry) }
            if actions.canDelete {
                Divider()
                Button("Delete for Everyone…", role: .destructive) { confirmingDelete = true }
            }
        }
        .confirmationDialog("Delete this message for everyone?", isPresented: $confirmingDelete) {
            Button("Delete for Everyone", role: .destructive, action: actions.delete)
        } message: {
            Text("Everyone in this chat sees that you deleted it.")
        }
    }

    /// Shown on hover, like Desktop's: Signal's reactions and any other emoji.
    @ViewBuilder private var reactButton: some View {
        if !message.deleted {
            Button {
                picking = true
            } label: {
                Image(systemName: "face.smiling").font(.system(size: 15)).foregroundStyle(Theme.secondaryText)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("React")
            .opacity(hovering || picking ? 1 : 0)
            .popover(isPresented: $picking) {
                ReactionPicker(mine: mine) { emoji in
                    picking = false
                    actions.react(emoji)
                }
            }
        }
    }

    private var bubble: some View {
        // files without a caption: the time sits under them on the right, the bubble as wide as they are
        let filesOnly = !files.isEmpty && (message.body ?? "").isEmpty && !message.deleted
        return VStack(alignment: filesOnly ? .trailing : .leading, spacing: 4) {
            if let q = message.quote, !message.deleted {
                VStack(alignment: .leading, spacing: 1) {
                    Text(q.author == message.author ? "Reply" : "Quoted").font(.system(size: 11, weight: .semibold))
                    Group {
                        if let text = q.text {
                            MessageText.text(
                                MessageText.attributed(text, ranges: q.ranges, name: mentionName), size: 12)
                        } else {
                            Text("Attachment")
                        }
                    }
                    .font(.system(size: 12)).lineLimit(2)
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
                    } else if let placeholder = Placeholder.text(message.kind) {
                        // something we can't show yet (sticker, contact, payment, poll, gift badge)
                        Text(placeholder).italic()
                    } else if let body = message.body, !body.isEmpty {
                        let text = MessageText.text(
                            MessageText.attributed(
                                body, ranges: message.ranges, name: mentionName, revealSpoilers: spoilersRevealed),
                            size: 14)
                        if MessageText.hasSpoiler(message.ranges) && !spoilersRevealed {
                            // as Desktop: a click shows what the spoilers hide
                            text.onTapGesture { spoilersRevealed = true }.help("Click to reveal")
                        } else {
                            text.textSelection(.enabled)
                        }
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
        .overlay {
            RoundedRectangle(cornerRadius: Theme.bubbleRadius).stroke(.yellow, lineWidth: highlighted ? 3 : 0)
                .animation(.easeOut(duration: 0.4), value: highlighted)
        }
    }

    private var meta: some View {
        HStack(spacing: 3) {
            if message.edited && !message.deleted { Text("edited") }
            if message.expiresIn > 0 {
                Image(systemName: "hourglass").accessibilityLabel("Disappears")
            }
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

    /// Each emoji with how many; ours tinted. A click shows who reacted.
    private var reactionPill: some View {
        Button {
            showingReactors = true
        } label: {
            HStack(spacing: 4) {
                ForEach(MessageActions.pill(reactions, me: actions.me), id: \.emoji) { entry in
                    Text(entry.count > 1 ? "\(entry.emoji) \(entry.count)" : entry.emoji).font(.system(size: 12))
                        .padding(.horizontal, entry.mine ? 4 : 0)
                        .background(entry.mine ? Theme.outgoing.opacity(0.45) : .clear, in: Capsule())
                }
            }
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(Theme.field, in: Capsule())
            .overlay(Capsule().stroke(Theme.window, lineWidth: 2))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(
            MessageActions.pill(reactions, me: actions.me).map { "\($0.emoji) \($0.count)" }.joined(separator: ", ")
        )
        .offset(y: -6)
        .popover(isPresented: $showingReactors) {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(reactions, id: \.reactor) { r in
                    HStack {
                        Text(r.emoji)
                        Text(r.reactor == actions.me ? "You" : actions.name(r.reactor))
                        Spacer(minLength: 16)
                        if r.reactor == actions.me {
                            Button("Remove") {
                                showingReactors = false
                                actions.react(r.emoji)
                            }
                            .controlSize(.small)
                        }
                    }
                }
            }
            .padding(12)
            .frame(minWidth: 200)
        }
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

/// Something that happened in the chat, centered between messages (Desktop's chat events).
struct ChatEventRow: View {
    let event: ChatEvent
    let name: String
    /// Our ACI, to word our own changes ("You set…").
    var me: String?
    let reset: () -> Void

    var body: some View {
        VStack(spacing: 6) {
            switch event.kind {
            case .decryptionFailed:
                // presage doesn't say whose side failed: worded for both
                Label("A message between you and \(name) couldn't be decrypted", systemImage: "exclamationmark.lock")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.secondaryText)
                if event.conversationId.hasPrefix("c:") && !event.conversationId.hasPrefix("c:PNI:") {
                    Button("Reset Secure Session", action: reset).controlSize(.small)
                }
            case .timerChanged:
                Label(
                    Placeholder.timerChange(event, who: event.author == me ? nil : name), systemImage: "hourglass"
                )
                .font(.system(size: 12))
                .foregroundStyle(Theme.secondaryText)
            case .sessionReset:
                Label("You reset the secure session with \(name)", systemImage: "lock.rotation")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.secondaryText)
            case .outgoingCall:
                Label(
                    Placeholder.call("Outgoing voice call", seconds: event.value), systemImage: "phone.arrow.up.right"
                )
                .font(.system(size: 12))
                .foregroundStyle(Theme.secondaryText)
            case .incomingCall:
                Label(
                    Placeholder.call("Incoming voice call", seconds: event.value), systemImage: "phone.arrow.down.left"
                )
                .font(.system(size: 12))
                .foregroundStyle(Theme.secondaryText)
            case .missedCall:
                Label("Missed voice call", systemImage: "phone.down")
                    .font(.system(size: 12))
                    .foregroundStyle(.red.opacity(0.9))
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
    }
}

/// Signal's reactions, and any emoji from the character palette ("More…").
struct ReactionPicker: View {
    let mine: String?
    let choose: (String) -> Void
    @State private var other = ""
    @FocusState private var otherFocused: Bool

    var body: some View {
        HStack(spacing: 4) {
            ForEach(MessageActions.quickReactions, id: \.self) { emoji in
                Button {
                    choose(emoji)
                } label: {
                    Text(emoji).font(.system(size: 22)).padding(4)
                        .background(emoji == mine ? Theme.outgoing.opacity(0.5) : .clear, in: Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(emoji == mine ? "\(emoji), yours: remove" : emoji)
            }
            Button {
                otherFocused = true  // the palette types into the focused field
                NSApp.orderFrontCharacterPalette(nil)
            } label: {
                Image(systemName: "plus.circle").font(.system(size: 20))
            }
            .buttonStyle(.plain)
            .accessibilityLabel("More reactions")
            // receives the emoji picked in the palette
            TextField("", text: $other).focused($otherFocused).frame(width: 1).opacity(0.01)
                .onChange(of: other) { _, new in
                    if let first = new.first, MessageActions.isReaction(String(first)) { choose(String(first)) }
                    other = ""
                }
        }
        .padding(8)
    }
}
