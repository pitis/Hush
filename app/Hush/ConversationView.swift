import QuickLook
import SwiftUI

struct ConversationView: View {
    let store: AppStore
    let conversation: Conversation
    @State private var quickLook: URL?
    @State private var dropTargeted = false

    var body: some View {
        TimelineList(store: store, conversation: conversation) { a in
            do {
                quickLook = try Files.openable(a)
                return true
            } catch {
                store.showError("Couldn't open \(a.fileName ?? "the file"): \(error.localizedDescription)")
                return false
            }
        }
        .environment(\.chatColor, conversation.chatColor)
        .environment(\.onWallpaper, conversation.wallpaper != nil)
        .background { if let w = conversation.wallpaper { Wallpaper(wallpaper: w) } }
        // floats over the timeline: messages scroll on under it
        .safeAreaBar(edge: .bottom) {
            if RequestBar.replacesComposer(conversation) {
                RequestBar(store: store, conversation: conversation)
            } else {
                ComposerView(store: store, conversationId: conversation.id)
            }
        }
        .modifier(ConversationChrome(store: store, conversation: conversation))
        .quickLookPreview($quickLook)
        // the preview closed: its link goes
        .onChange(of: quickLook) { old, _ in if let old { Files.close(old) } }
        .onDisappear { if let quickLook { Files.close(quickLook) } }
        // files dropped anywhere on the chat wait in the composer's tray
        .dropDestination(for: URL.self) { urls, _ in
            guard !RequestBar.replacesComposer(conversation) else { return false }
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

/// In place of the composer, as Desktop's: a message request's Accept, Delete and Block, a group
/// invitation's Join, Decline and Block, or a blocked chat's Unblock and Delete. Block, Delete
/// and Decline ask first.
struct RequestBar: View {
    let store: AppStore
    let conversation: Conversation
    @State private var confirming: Confirm?
    @State private var answering = false

    private enum Confirm: Identifiable {
        case block, delete, decline, cancelRequest
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
                } else if conversation.groupJoinRequested {
                    // here only, should cancelling fail for good (the request stays)
                    Button("Delete") { confirming = .delete }
                    Button("Cancel Request") { confirming = .cancelRequest }.disabled(answering)
                } else if conversation.groupInvitation {
                    Button("Block") { confirming = .block }
                    Button("Decline") { confirming = .decline }.disabled(answering)
                    Button("Join") { answer(accept: true) }
                        .keyboardShortcut(.defaultAction).disabled(answering)
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
            confirming == .block
                ? RequestBar.blockTitle(conversation, name: name)
                : confirming == .decline
                    ? "Decline the invitation?"
                    : confirming == .cancelRequest ? "Cancel your request to join?" : "Delete this chat?",
            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
            presenting: confirming
        ) { c in
            switch c {
            case .block: Button("Block", role: .destructive) { store.block(conversation.id) }
            case .delete: Button("Delete", role: .destructive) { store.deleteChat(conversation.id) }
            case .decline: Button("Decline", role: .destructive) { answer(accept: false) }
            case .cancelRequest: Button("Cancel Request", role: .destructive) { cancelRequest() }
            }
        } message: { c in
            switch c {
            case .block: Text(RequestBar.blockMessage(conversation))
            case .decline: Text("Its members see that you declined, and the chat is deleted from this Mac.")
            case .cancelRequest: Text("The chat is deleted from this Mac. You can ask again with the link.")
            case .delete:
                Text(
                    isGroup
                        ? "Its messages are deleted from this Mac."
                        : "Its messages are deleted from this Mac and your phone.")
            }
        }
    }

    /// Joins or declines; a failure shows.
    private func answer(accept: Bool) {
        answering = true
        Task {
            if let failure = await store.answerInvitation(conversation.id, accept: accept) { store.showError(failure) }
            answering = false
        }
    }

    /// Withdraws our request to join; a failure shows.
    private func cancelRequest() {
        answering = true
        Task {
            if let failure = await store.cancelJoinRequest(conversation.id) { store.showError(failure) }
            answering = false
        }
    }

    /// A blocked chat, a request or an invitation to answer: the bar, not the composer.
    nonisolated static func replacesComposer(_ c: Conversation) -> Bool {
        c.blocked || c.messageRequest || c.groupInvitation || c.groupJoinRequested
    }

    /// What the bar says above its buttons.
    nonisolated static func prompt(_ c: Conversation, name: String) -> String {
        if c.blocked {
            return c.kind == .group ? "You blocked this group." : "You blocked \(name)."
        }
        if c.groupJoinRequested { return "You asked to join this group. An admin has to let you in." }
        if c.groupInvitation {
            return "You're invited to join this group. Its members see your name and picture once you join."
        }
        return c.kind == .group
            ? "Join this group? They won't know you've seen their messages until you accept."
            : "Let \(name) message you? They won't know you've seen their messages until you accept."
    }

    nonisolated static func blockTitle(_ c: Conversation, name: String) -> String {
        c.kind == .group ? "Block this group?" : "Block \(name)?"
    }

    nonisolated static func blockMessage(_ c: Conversation) -> String {
        if c.groupInvitation { return "You won't get its messages or updates. The invitation stays unanswered." }
        return c.kind == .group
            ? "You won't get its messages or updates. You stay a member: leave it from your phone."
            : "They won't be able to message or call you."
    }
}

/// The chat's name, subtitle and actions in the window toolbar; a modifier of its own so typing
/// and connection changes don't re-render the timeline.
struct ConversationChrome: ViewModifier {
    let store: AppStore
    let conversation: Conversation
    @State private var choosingColor = false
    @State private var showingGroupInfo = false

    /// A group's timer changes with the group (from the phone); a blocked or unaccepted chat
    /// has nothing to send it to.
    private var canSetTimer: Bool {
        conversation.kind == .direct && !conversation.blocked && !conversation.messageRequest
            && !conversation.id.hasPrefix("c:PNI:")
    }

    private var canCall: Bool {
        conversation.kind == .direct && !conversation.blocked && !conversation.messageRequest
            && !conversation.id.hasPrefix("c:PNI:")
            && conversation.id != store.ownAci.map { "c:\($0)" }
    }

    func body(content: Content) -> some View {
        content
            .navigationTitle(store.title(conversation))
            .navigationSubtitle(
                Self.subtitle(
                    typing: store.isTyping(conversation.id), connection: store.connection,
                    timer: Int64(conversation.expireTimer),
                    username: conversation.kind == .direct ? conversation.username : nil)
            )
            .toolbar {
                ToolbarItemGroup(placement: .primaryAction) {
                    if canCall { callButton }
                    optionsMenu
                }
            }
            .sheet(isPresented: $showingGroupInfo) { GroupInfoView(store: store, conversation: conversation) }
    }

    private var callButton: some View {
        let inCall = store.call != nil && store.call?.phase != .ended
        return Button {
            if inCall { store.showCallWindow() } else { store.startCall(conversation.id) }
        } label: {
            Label(inCall ? "Show the Call" : "Voice Call", systemImage: inCall ? "phone.fill" : "phone")
        }
        // signaling rides the message pipeline: no call before it is connected
        .disabled(!inCall && store.connection != .online)
        .help(inCall ? "Show the call" : "Voice call")
    }

    private var optionsMenu: some View {
        Menu {
            if canSetTimer {
                Picker(
                    "Disappearing Messages",
                    selection: Binding(
                        get: { conversation.expireTimer },
                        set: { store.setTimer(conversation.id, seconds: $0) })
                ) {
                    ForEach(Placeholder.timerChoices(current: conversation.expireTimer), id: \.self) {
                        Text(Placeholder.timerChoice($0)).tag($0)
                    }
                }
                .pickerStyle(.menu)
            }
            Button("Chat Colour and Wallpaper…") { choosingColor = true }
            Button("Search in Chat") { store.searchInChat() }
            if conversation.kind == .group { Button("Group Info…") { showingGroupInfo = true } }
            // the note written about them on the phone
            if let note = conversation.note {
                Divider()
                Text("Note: \(note)")
            }
        } label: {
            Label("Chat Options", systemImage: "info.circle")
        }
        .menuIndicator(.hidden)
        .help("Chat options")
        .popover(isPresented: $choosingColor, arrowEdge: .bottom) {
            ChatStylePicker(
                chatColor: conversation.chatColor, wallpaper: conversation.wallpaper,
                chooseColor: { store.setChatColor(conversation.id, $0) },
                chooseWallpaper: { store.setWallpaper(conversation.id, $0) })
        }
    }

    /// Under the chat's name: what matters most now, in words ("" for nothing).
    nonisolated static func subtitle(typing: Bool, connection: ConnectionState, timer: Int64, username: String?)
        -> String
    {
        if typing { return "typing…" }
        switch connection {
        case .offline: return "Offline"
        case .connecting: return "Connecting…"
        case .online, .unlinked: break
        }
        if timer > 0 { return "Disappearing messages: \(Placeholder.duration(timer))" }
        return username.map { "@\($0)" } ?? ""
    }
}

struct TimelineList: View {
    let store: AppStore
    let conversation: Conversation
    /// Shows a file; false if it couldn't be.
    let open: (Attachment) -> Bool
    @State private var atBottom = true
    @State private var nearTop = false
    @State private var unseenNew = false
    @State private var position = ScrollPosition(edge: .bottom)
    @Environment(\.controlActiveState) private var activeState

    /// What the timeline follows of its scroll geometry.
    private struct Edges: Equatable {
        var atBottom: Bool
        var nearTop: Bool
        var contentHeight: CGFloat
    }

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
                                .wallpaperBacked()
                                .padding(.vertical, 10)
                        case .event(let e):
                            ChatEventRow(event: e, name: name(e), me: store.ownAci)
                        case .failures(let e, let count, let reset):
                            // presage resets sessions by ACI only
                            let direct = e.conversationId.hasPrefix("c:") && !e.conversationId.hasPrefix("c:PNI:")
                            DecryptionNotice(
                                name: name(e), count: count, canReset: direct && !reset,
                                resetting: store.resettingSessions.contains(e.conversationId),
                                reset: { store.resetSession(e.conversationId) })
                        case .message(let m, let first, let last):
                            let key = MessageKey(author: m.author, sentAt: m.sentAt)
                            MessageRow(
                                message: m, first: first, last: last, isGroup: isGroup,
                                senderName: store.senderName(m.author), mentionName: store.mentionName,
                                reactions: store.reactions[key] ?? [],
                                files: store.attachments[key] ?? [], preview: store.previews[key],
                                poll: store.polls[key],
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
                                    viewOnce: { Task { await store.viewOnceOpened(m) } },
                                    vote: { options in Task { await store.vote(m, options: options) } },
                                    endPoll: { Task { await store.endPoll(m) } },
                                    name: store.senderName))
                        }
                    }
                    TypingRow(store: store, conversationId: conversation.id, isGroup: isGroup)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            .scrollPosition($position)
            .defaultScrollAnchor(.bottom)
            .onScrollGeometryChange(for: Edges.self) { g in
                Edges(
                    atBottom: Timeline.isAtBottom(g), nearTop: Timeline.isNearTop(offsetY: g.contentOffset.y),
                    contentHeight: g.contentSize.height)
            } action: { old, new in
                // what grows at the bottom (a page's files and previews, the typing bubble) stays in
                // view: the bottom anchor doesn't follow it
                let follow = old.atBottom && new.contentHeight > old.contentHeight
                if follow { position.scrollTo(edge: .bottom) }
                atBottom = follow || new.atBottom
                if atBottom { unseenNew = false }
                nearTop = new.nearTop
            }
            .onChange(of: nearTop) { _, near in if near { loadOlder() } }
            .onChange(of: store.messages.first?.sentAt) { old, _ in
                // still at the top after a page arrived (short messages): keep paging. Not for a
                // chat's first page: the empty timeline before it counts as at the top.
                if nearTop, old != nil { loadOlder() }
            }
            .onChange(of: store.messages.last?.sentAt) { _, _ in
                let own = store.messages.last?.isOutgoing ?? false
                switch Timeline.onNewLastMessage(atBottom: atBottom, isOwn: own) {
                case .scrollToBottom:
                    withAnimation(.easeOut(duration: 0.15)) { position.scrollTo(edge: .bottom) }
                    unseenNew = false
                case .showPill: unseenNew = true
                case .nothing: break
                }
            }
            .onChange(of: conversation.id) { _, _ in
                unseenNew = false
                position.scrollTo(edge: .bottom)
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
                        position.scrollTo(edge: .bottom)
                        unseenNew = false
                    } label: {
                        Label("New messages", systemImage: "arrow.down")
                    }
                    .buttonStyle(.glassProminent)
                    .padding(16)
                }
            }
        }
    }

    /// Who an event is about: the chat's own name in a direct chat (never an id).
    private func name(_ e: ChatEvent) -> String {
        conversation.kind == .group ? store.senderName(e.author) : store.title(conversation)
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
    var poll: Poll?
    /// Just opened from search: outlined for a moment.
    var highlighted = false
    let reply: () -> Void
    let retry: () -> Void
    let download: (Int64) -> Void
    /// Shows a file; false if it couldn't be.
    let open: (Attachment) -> Bool
    var actions = Actions()
    @Environment(\.chatColor) private var chatColor
    @State private var hovering = false
    @State private var picking = false
    @State private var showingReactors = false
    @State private var confirmingDelete = false
    @State private var spoilersRevealed = false
    /// View-once media was opened here; the core's word that it's viewed is on its way.
    @State private var viewOnceOpening = false

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
        /// View-once media was opened.
        var viewOnce: () -> Void = {}
        var vote: ([UInt32]) -> Void = { _ in }
        var endPoll: () -> Void = {}
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
                        .wallpaperBacked()
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
            if !message.deleted && !isViewOnce && !isSticker {
                AttachmentList(files: files, download: download, open: { _ = open($0) })
                if let preview { LinkPreviewCard(preview: preview) }
            }
            HStack(alignment: .lastTextBaseline, spacing: 8) {
                Group {
                    if message.deleted {
                        Text("This message was deleted").italic().foregroundStyle(.secondary)
                    } else if isViewOnce {
                        viewOnce
                    } else if let sticker = stickerImage {
                        ThumbnailImage(path: sticker).scaledToFit().frame(maxWidth: 160, maxHeight: 160)
                            .accessibilityLabel(Placeholder.text(message.kind) ?? "Sticker")
                    } else if case .poll(let question) = message.kind, let poll {
                        PollCard(
                            question: question, poll: poll, me: actions.me, mine: message.author == actions.me,
                            vote: actions.vote, end: actions.endPoll)
                    } else if let placeholder = Placeholder.text(message.kind) {
                        // something we can't show yet (sticker, contact, payment, poll, gift badge)
                        HStack(spacing: 6) {
                            Text(placeholder).italic()
                            // a sticker whose image didn't come: fetch it again
                            if let file = Placeholder.stickerFile(message, files: files) {
                                DownloadState(state: file.state, download: { download(file.id) })
                            }
                        }
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
                // a sticker has no bubble: its time and ticks would sit on a wallpaper
                if stickerImage != nil { meta.wallpaperBacked() } else { meta }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .foregroundStyle(outgoing ? .white : Theme.incomingText)
        // as Desktop: a sticker stands on its own, without a bubble
        .background(
            stickerImage != nil
                ? AnyShapeStyle(.clear) : outgoing ? ChatColors.style(chatColor) : AnyShapeStyle(Theme.incoming),
            in: RoundedRectangle(cornerRadius: Theme.bubbleRadius)
        )
        .overlay {
            RoundedRectangle(cornerRadius: Theme.bubbleRadius).stroke(.yellow, lineWidth: highlighted ? 3 : 0)
                .animation(.easeOut(duration: 0.4), value: highlighted)
        }
    }

    private var isSticker: Bool {
        if case .sticker = message.kind { return true }
        return false
    }

    private var stickerImage: String? {
        message.deleted ? nil : Placeholder.stickerImage(message, files: files)
    }

    private var isViewOnce: Bool {
        if case .viewOnce = message.kind { return true }
        return false
    }

    @ViewBuilder private var viewOnce: some View {
        let state = ViewOnce.state(message, files: files)
        let label = Label(ViewOnce.label(state), systemImage: "circle.dashed")
        switch state {
        case .open(let file):
            Button {
                // linked for the viewer first: the core drops the file next
                viewOnceOpening = ViewOnce.open(file, with: open, then: actions.viewOnce)
            } label: {
                label
            }
            .buttonStyle(.bordered)
            .disabled(viewOnceOpening)  // one look: a second click would relink what's showing
        case .download(let id):
            Button {
                download(id)
            } label: {
                label
            }.buttonStyle(.bordered)
        case .sent, .viewed, .downloading:
            label.italic()
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
        .foregroundStyle(.secondary)
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
            .overlay(Capsule().stroke(Color(nsColor: .windowBackgroundColor), lineWidth: 2))
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

    var body: some View {
        VStack(spacing: 6) {
            switch event.kind {
            case .decryptionFailed:
                EmptyView()  // the timeline collapses these into a `DecryptionNotice`
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
        .wallpaperBacked()
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
    }
}

/// Someone typing, as the last thing in the timeline; a view of its own so typing doesn't
/// re-render the messages.
struct TypingRow: View {
    let store: AppStore
    let conversationId: String
    let isGroup: Bool

    var body: some View {
        if let who = store.typers(conversationId).first {
            TypingBubble(typer: isGroup ? (who, store.senderName(who)) : nil).padding(.top, 6)
        }
    }
}

/// An incoming bubble of three pulsing dots (still under Reduce Motion), after the typer's
/// picture in a group.
struct TypingBubble: View {
    /// Who, in a group (ACI and name).
    let typer: (id: String, name: String)?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            if let typer { Avatar(id: "c:\(typer.id)", name: typer.name, size: 28) }
            TimelineView(.animation(minimumInterval: 1 / 30, paused: reduceMotion)) { context in
                let t = context.date.timeIntervalSinceReferenceDate
                HStack(spacing: 4) {
                    ForEach(0..<3, id: \.self) { i in
                        // a wave from left to right, once every 1.25s
                        let wave = max(0, sin(t * .pi * 1.6 - Double(i) * 0.7))
                        Circle().frame(width: 7, height: 7).opacity(reduceMotion ? 0.6 : 0.35 + 0.65 * wave)
                    }
                }
            }
            .foregroundStyle(Theme.secondaryText)
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
            .background(Theme.incoming, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
            Spacer(minLength: 80)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(typer.map { "\($0.name) is typing" } ?? "Typing")
    }
}

/// Messages that couldn't be decrypted, counted, with one way to start the session over.
struct DecryptionNotice: View {
    let name: String
    let count: Int
    /// A direct chat whose session hasn't been reset since.
    let canReset: Bool
    /// A reset is on its way (the store's: a failure offers it again).
    let resetting: Bool
    let reset: () -> Void
    @State private var confirming = false

    var body: some View {
        VStack(spacing: 4) {
            Label(Self.text(count: count, name: name), systemImage: "exclamationmark.lock")
                .font(.system(size: 12))
                .foregroundStyle(Theme.secondaryText)
            if canReset {
                Button(resetting ? "Resetting…" : "Reset Secure Session…") { confirming = true }
                    .buttonStyle(.link).font(.system(size: 12))
                    .disabled(resetting)
            }
        }
        .wallpaperBacked()
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
        .confirmationDialog("Reset the secure session with \(name)?", isPresented: $confirming) {
            Button("Reset", action: reset)
        } message: {
            Text(
                "Hush starts a new encrypted session. \(name) will see that it was reset. Messages that "
                    + "couldn't be decrypted stay unreadable: ask them to resend anything important.")
        }
    }

    /// Their failures (retry receipts) show as ours do: worded for both.
    nonisolated static func text(count: Int, name: String) -> String {
        count == 1
            ? "A message between you and \(name) couldn't be decrypted"
            : "\(count) messages between you and \(name) couldn't be decrypted"
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
