import SwiftUI

struct SidebarView: View {
    @Bindable var store: AppStore
    @FocusState private var searchFocused: Bool
    /// The message result picked with the arrows while browsing results.
    @State private var resultIndex: Int?
    /// The chat whose "Block…" waits for confirmation.
    @State private var blocking: Conversation?
    @State private var findingUsername = false
    @State private var makingGroup = false
    @State private var joiningByLink = false
    @State private var showingStories = false

    private var searching: Bool { store.isSearching }
    /// The archive is on show (a search covers every chat, so it counts as the inbox).
    private var inArchive: Bool { store.showingArchive && !searching }
    /// Arrows step through message results (in a chat search, or when no chat matches) rather
    /// than chats.
    private var browsingResults: Bool {
        searching && (store.searchScope != nil || store.visibleConversations.isEmpty)
    }

    var body: some View {
        ScrollViewReader { proxy in
            List(selection: selection) {
                if let scope = store.searchScope { scopeRow(scope) }
                if store.searchScope == nil {
                    chatRows
                    if !searching, !store.showingArchive, store.archivedCount > 0 { archiveRow }
                }
                if searching { messageResults }
            }
            .listStyle(.sidebar)
            // arrows step through message results while browsing them; through chats natively
            .onKeyPress(.downArrow) { browsingResults ? moveInSearch(1) : .ignored }
            .onKeyPress(.upArrow) { browsingResults ? moveInSearch(-1) : .ignored }
            .onChange(of: store.selectedId) { _, id in
                // keyboard navigation can select a chat that is scrolled out of view
                if let id, !browsingResults { proxy.scrollTo(id) }
            }
            .onChange(of: resultIndex) { _, i in if let i { proxy.scrollTo("result-\(i)") } }
        }
        // like Desktop's search: arrows pick a chat or message, ↩ opens it, Esc ends the search
        .searchable(text: $store.searchQuery, placement: .sidebar, prompt: "Search")
        .searchFocused($searchFocused)
        .onKeyPress(.downArrow) { searchFocused ? moveInSearch(1) : .ignored }
        .onKeyPress(.upArrow) { searchFocused ? moveInSearch(-1) : .ignored }
        .onSubmit(of: .search) {
            if browsingResults, let i = resultIndex, store.searchResults.indices.contains(i) {
                Task { await store.open(store.searchResults[i]) }
            } else {
                store.openSearchResult()
            }
        }
        // Esc in an empty field leaves it: the search (and its chat scope) ends
        .onChange(of: searchFocused) { _, focused in
            if !focused, store.searchQuery.isEmpty { store.endSearch() }
        }
        .safeAreaInset(edge: .top, spacing: 0) { ErrorBanner(store: store) }
        .safeAreaInset(edge: .bottom, spacing: 0) { ConnectionStatus(store: store) }
        .toolbar {
            if inArchive {
                ToolbarItem(placement: .navigation) {
                    Button {
                        store.showArchive(false)
                    } label: {
                        Label("Back to Chats", systemImage: "chevron.left")
                    }
                    .help("Back to chats")
                }
            } else {
                ToolbarItem {
                    Menu {
                        Button("New Chat by Username…") { findingUsername = true }
                        Button("New Group…") { makingGroup = true }
                        Button("Join Group by Link…") { joiningByLink = true }
                    } label: {
                        Label("New Chat", systemImage: "square.and.pencil")
                    }
                    .menuIndicator(.hidden)
                    .help("New chat")
                }
                if !store.stories.isEmpty {
                    ToolbarItem {
                        let unseen = StoryList.unseenCount(store.stories, me: store.ownAci)
                        Button {
                            showingStories = true
                        } label: {
                            Label(
                                unseen > 0 ? "Stories, \(unseen) new" : "Stories",
                                systemImage: unseen > 0 ? "circle.dashed.inset.filled" : "circle.dashed")
                        }
                        .help(unseen > 0 ? "Stories, \(unseen) new" : "Stories")
                    }
                }
            }
        }
        .sheet(isPresented: $findingUsername) { FindUsernameSheet(store: store) }
        .sheet(isPresented: $makingGroup) { NewGroupSheet(store: store) }
        .sheet(isPresented: $joiningByLink) { JoinGroupSheet(store: store) }
        .sheet(isPresented: $showingStories) { StoriesView(store: store) }
        .confirmationDialog(
            blocking.map { RequestBar.blockTitle($0, name: store.title($0)) } ?? "",
            isPresented: Binding(get: { blocking != nil }, set: { if !$0 { blocking = nil } }),
            presenting: blocking
        ) { c in
            Button("Block", role: .destructive) { store.block(c.id) }
        } message: { c in
            Text(RequestBar.blockMessage(c))
        }
        .onChange(of: store.searchFocusRequest) { _, _ in searchFocused = true }
        .onChange(of: store.searchResults) { _, _ in resultIndex = nil }
    }

    /// The list's selection is the open chat. A click puts the caret in its composer; arrows keep
    /// focus in the list.
    private var selection: Binding<String?> {
        Binding(
            get: { store.selectedId },
            set: { id in
                guard let id, id != store.selectedId else { return }
                store.select(id)
                if [.leftMouseDown, .leftMouseUp].contains(NSApp.currentEvent?.type) { store.focusComposer() }
            })
    }

    /// Searching one chat (⇧⌘F): which, and the way back to every chat.
    private func scopeRow(_ scope: String) -> some View {
        let title = store.conversations.first { $0.id == scope }.map(store.title) ?? "this chat"
        return HStack {
            Label("In \(title)", systemImage: "text.magnifyingglass").lineLimit(1)
            Spacer(minLength: 6)
            Button("All Chats", action: store.endSearch).buttonStyle(.link)
        }
        .font(.callout)
        .foregroundStyle(.secondary)
    }

    @ViewBuilder private var messageResults: some View {
        if store.searchResults.isEmpty {
            Group {
                if store.searchPending {
                    Text("Searching…")
                } else if let failed = store.searchFailed {
                    Text("Search failed: \(failed)").foregroundStyle(.orange)
                } else {
                    Text("No messages found")
                }
            }
            .font(.callout).foregroundStyle(.secondary)
        } else {
            Section("Messages") {
                ForEach(Array(store.searchResults.enumerated()), id: \.offset) { i, hit in
                    Button {
                        resultIndex = i
                        Task { await store.open(hit) }
                    } label: {
                        SearchResultRow(
                            hit: hit, chatTitle: chatTitle(hit.message.conversationId),
                            sender: hit.message.isOutgoing ? "You" : store.senderName(hit.message.author),
                            selected: resultIndex == i)
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(resultIndex == i ? .isSelected : [])
                    .id("result-\(i)")
                }
            }
        }
    }

    /// Pinned chats under their own title when there are any (as Desktop), then the rest.
    @ViewBuilder private var chatRows: some View {
        let list = store.visibleConversations
        let pinned = list.filter { $0.pinnedOrder != nil }
        if !searching, !store.showingArchive, !pinned.isEmpty {
            Section("Pinned") { ForEach(pinned, id: \.id) { c in chatRow(c) } }
            Section("Chats") { ForEach(list.filter { $0.pinnedOrder == nil }, id: \.id) { c in chatRow(c) } }
        } else if inArchive {
            Section("Archived") { ForEach(list, id: \.id) { c in chatRow(c) } }
        } else if searching && !list.isEmpty {
            Section("Chats") { ForEach(list, id: \.id) { c in chatRow(c) } }
        } else {
            ForEach(list, id: \.id) { c in chatRow(c) }
        }
    }

    /// The way into the archive, at the bottom of the inbox.
    private var archiveRow: some View {
        Button {
            store.showArchive(true)
        } label: {
            Label {
                HStack {
                    Text("Archived Chats")
                    Spacer(minLength: 6)
                    Text("\(store.archivedCount)").foregroundStyle(.secondary)
                }
            } icon: {
                Image(systemName: "archivebox")
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func chatRow(_ c: Conversation) -> some View {
        ConversationRow(
            conversation: c, title: store.title(c),
            line: ChatList.line(
                c, selected: c.id == store.selectedId, typing: store.isTyping(c.id), draft: Composer.draft(for: c.id),
                authorName: store.senderName(c.lastMessageAuthor), name: store.mentionName)
        )
        .tag(c.id)
        .contextMenu { chatMenu(c) }
        .id(c.id)
    }

    /// Right-click on a chat: Desktop's chat actions.
    @ViewBuilder private func chatMenu(_ c: Conversation) -> some View {
        if c.pinnedOrder == nil {
            // the core refuses a fifth pin; better not to offer it
            Button("Pin chat") { store.setPinned(c.id, true) }
                .disabled(store.conversations.filter { $0.pinnedOrder != nil }.count >= ChatList.maxPinned)
        } else {
            Button("Unpin chat") { store.setPinned(c.id, false) }
        }
        if ChatList.isMuted(c, now: Date()) {
            Button("Unmute notifications") { store.unmute(c.id) }
        } else {
            Menu("Mute notifications") {
                ForEach(ChatList.MuteOption.allCases, id: \.self) { option in
                    Button(option.title) { store.mute(c.id, for: option) }
                }
            }
        }
        if ChatList.isUnread(c) {
            Button("Mark as read") { store.markRead(c.id) }
        } else {
            Button("Mark as unread") { store.setMarkedUnread(c.id, true) }
        }
        Button("Export chat…") { export(c) }
        Divider()
        if c.archived {
            Button("Unarchive chat") { store.setArchived(c.id, false) }
        } else {
            Button("Archive chat") { store.setArchived(c.id, true) }
        }
        if c.id != store.ownAci.map({ "c:\($0)" }) {
            if c.blocked {
                Button("Unblock") { store.acceptRequest(c.id) }
            } else {
                Button("Block…", role: .destructive) { blocking = c }
            }
        }
    }

    /// Asks where, then exports the chat there and shows the new folder.
    private func export(_ c: Conversation) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.prompt = "Export"
        panel.message = "Choose where to put the chat: a readable page and its files, in a new folder."
        guard panel.runModal() == .OK, let folder = panel.url else { return }
        Task {
            if let made = await store.exportChat(c.id, to: folder) {
                NSWorkspace.shared.activateFileViewerSelecting([made])
            }
        }
    }

    private func chatTitle(_ id: String) -> String {
        store.conversations.first { $0.id == id }.map(store.title) ?? String(id.dropFirst(2).prefix(8))
    }

    private func move(_ step: Int) -> KeyPress.Result {
        store.selectNeighbor(step: step, unreadOnly: false)
        return .handled
    }

    /// In the search field: through message results when browsing them (each opens as it is
    /// picked, like stepping through matches), else through the chats.
    private func moveInSearch(_ step: Int) -> KeyPress.Result {
        guard browsingResults, !store.searchResults.isEmpty else { return move(step) }
        let last = store.searchResults.count - 1
        let next = resultIndex.map { min(max($0 + step, 0), last) } ?? (step > 0 ? 0 : last)
        resultIndex = next
        Task { await store.open(store.searchResults[next]) }
        return .handled
    }
}

/// A message found by search: its chat, when, who wrote it, the matching text highlighted.
struct SearchResultRow: View {
    let hit: SearchHit
    let chatTitle: String
    let sender: String
    let selected: Bool

    var body: some View {
        let m = hit.message
        HStack(alignment: .top, spacing: 12) {
            Avatar(id: m.conversationId, name: chatTitle, size: 36)
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(chatTitle).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                    Spacer(minLength: 6)
                    TimelineView(.everyMinute) { context in
                        Text(Formatting().listTime(m.sentAt, now: context.date)).font(.system(size: 11))
                            .foregroundStyle(Theme.secondaryText)
                    }
                }
                let who = m.conversationId.hasPrefix("g:") || m.isOutgoing ? "\(sender): " : ""
                Text("\(Text(who).foregroundStyle(Theme.secondaryText))\(Text(SearchText.highlighted(hit.snippet)))")
                    .font(.system(size: 12))
            }
            .lineLimit(2)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .background(selected ? Theme.selection : .clear, in: RoundedRectangle(cornerRadius: 10))
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
    }
}

/// A chat in the list; the list draws the selection.
struct ConversationRow: View {
    let conversation: Conversation
    let title: String
    let line: ChatList.Line

    var body: some View {
        let unread = ChatList.isUnread(conversation)
        HStack(spacing: 10) {
            Avatar(id: conversation.id, name: title, size: 40)
            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    Text(title).font(.system(size: 13, weight: unread ? .bold : .semibold)).lineLimit(1)
                    Spacer(minLength: 6)
                    if ChatList.isMuted(conversation, now: Date()) {
                        Image(systemName: "bell.slash.fill").font(.system(size: 10))
                            .foregroundStyle(Theme.secondaryText).accessibilityLabel("Muted")
                    }
                    // relative times ("Now", "5m", "Yesterday"…) move on by themselves
                    TimelineView(.everyMinute) { context in
                        Text(Formatting().listTime(conversation.lastMessageAt, now: context.date))
                            .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                    }
                }
                HStack(spacing: 4) {
                    secondLine.font(.system(size: 13)).lineLimit(1)
                    Spacer(minLength: 6)
                    if conversation.unreadCount > 0 {
                        Text("\(conversation.unreadCount)").font(.system(size: 11, weight: .bold))
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Color.accentColor, in: Capsule())
                            .foregroundStyle(.white)
                    } else if conversation.markedUnread {
                        // marked by hand: a dot, no count (as Desktop)
                        Circle().fill(Color.accentColor).frame(width: 8, height: 8).accessibilityLabel("Unread")
                    }
                }
                .foregroundStyle(unread ? .primary : Theme.secondaryText)
            }
        }
        .padding(.vertical, 4)
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder private var secondLine: some View {
        switch line {
        case .typing:
            Text("typing…").italic()
        case .draft(let text):
            Text("\(Text("Draft: ").italic().bold())\(text)")
        case .message(let text, let tick):
            if let tick { TickIcon(tick: tick) }
            Text(text).italic(conversation.lastMessageDeleted)
        }
    }
}

/// Finds someone by their username ("name.42") and opens a chat with them.
struct FindUsernameSheet: View {
    let store: AppStore
    @State private var username = ""
    @State private var problem: String?
    @State private var searching = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New chat").font(.system(size: 17, weight: .semibold))
            Text("Enter their username, as it shows in their profile.")
                .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
            TextField("name.42", text: $username)
                .textFieldStyle(.roundedBorder)
                .onSubmit(find)
                .disabled(searching)
            if let problem { Text(problem).font(.system(size: 12)).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Find", action: find).keyboardShortcut(.defaultAction)
                    .disabled(username.trimmingCharacters(in: .whitespaces).isEmpty || searching)
            }
        }
        .padding(20)
        .frame(width: 360)
    }

    private func find() {
        searching = true
        problem = nil
        Task {
            problem = await store.findUsername(username)
            searching = false
            if problem == nil { dismiss() }
        }
    }
}
