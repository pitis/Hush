import SwiftUI

struct SidebarView: View {
    @Bindable var store: AppStore
    @FocusState private var searchFocused: Bool
    @FocusState private var listFocused: Bool
    /// The message result picked with the arrows while browsing results.
    @State private var resultIndex: Int?

    private var searching: Bool { !store.searchQuery.trimmingCharacters(in: .whitespaces).isEmpty }
    /// Arrows step through message results (in a chat search, or when no chat matches) rather
    /// than chats.
    private var browsingResults: Bool {
        searching && (store.searchScope != nil || store.visibleConversations.isEmpty)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Chats").font(.system(size: 22, weight: .bold))
                Spacer()
            }
            .padding(.horizontal, 18)
            .padding(.top, 14)
            .padding(.bottom, 10)
            searchField
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        if store.searchScope == nil {
                            if searching && !store.visibleConversations.isEmpty { SectionTitle("Chats") }
                            ForEach(store.visibleConversations, id: \.id) { c in chatRow(c) }
                        }
                        if searching { messageResults }
                    }
                    .padding(.horizontal, 8)
                }
                .focusable()
                .focused($listFocused)
                .focusEffectDisabled()
                .onKeyPress(.downArrow) { moveInSearch(1) }
                .onKeyPress(.upArrow) { moveInSearch(-1) }
                .onChange(of: store.selectedId) { _, id in
                    // keyboard navigation can select a chat that is scrolled out of view
                    if let id, !browsingResults { proxy.scrollTo(id) }
                }
                .onChange(of: resultIndex) { _, i in if let i { proxy.scrollTo("result-\(i)") } }
            }
        }
        .background(Theme.sidebar)
        .onChange(of: store.searchFocusRequest) { _, _ in searchFocused = true }
        .onChange(of: store.searchResults) { _, _ in resultIndex = nil }
    }

    private var searchField: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass").foregroundStyle(Theme.secondaryText)
            if let scope = store.searchScope {
                let title = store.conversations.first { $0.id == scope }.map(store.title) ?? "this chat"
                HStack(spacing: 3) {
                    Text("In \(title)").font(.system(size: 12, weight: .medium)).lineLimit(1)
                    Button(action: store.endSearch) { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Search all chats")
                }
                .padding(.horizontal, 7).padding(.vertical, 2)
                .background(Theme.selection, in: Capsule())
            }
            TextField("Search", text: $store.searchQuery).textFieldStyle(.plain).focused($searchFocused)
                // like Desktop's search: arrows pick a chat or message, ↩ opens it, Esc ends the search
                .onKeyPress(.downArrow) { moveInSearch(1) }
                .onKeyPress(.upArrow) { moveInSearch(-1) }
                .onKeyPress(.return) {
                    if browsingResults, let i = resultIndex, store.searchResults.indices.contains(i) {
                        Task { await store.open(store.searchResults[i]) }
                    } else {
                        store.openSearchResult()
                    }
                    return .handled
                }
                .onExitCommand {
                    store.endSearch()
                    searchFocused = false
                }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .background(Theme.field, in: RoundedRectangle(cornerRadius: 8))
        .padding(.horizontal, 14)
        .padding(.bottom, 8)
    }

    @ViewBuilder private var messageResults: some View {
        if store.searchResults.isEmpty {
            Group {
                if store.searchPending {
                    Text("Searching…")
                } else if let failed = store.searchFailed {
                    Text("Search failed: \(failed)").foregroundStyle(.yellow)
                } else {
                    Text("No messages found")
                }
            }
            .font(.system(size: 13)).foregroundStyle(Theme.secondaryText)
            .padding(.horizontal, 10).padding(.vertical, 12)
        } else {
            SectionTitle("Messages")
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

    private func chatRow(_ c: Conversation) -> some View {
        let selected = c.id == store.selectedId
        return Button {
            store.select(c.id)
            store.focusComposer()
        } label: {
            ConversationRow(
                conversation: c, title: store.title(c), selected: selected,
                line: ChatList.line(
                    c, selected: selected, typing: store.isTyping(c.id), draft: Composer.draft(for: c.id),
                    authorName: store.senderName(c.lastMessageAuthor))
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .id(c.id)
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

struct SectionTitle: View {
    let title: String
    init(_ title: String) { self.title = title }

    var body: some View {
        Text(title).font(.system(size: 12, weight: .semibold)).foregroundStyle(Theme.secondaryText)
            .padding(.horizontal, 10).padding(.top, 10).padding(.bottom, 2)
            .accessibilityAddTraits(.isHeader)
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
                (m.conversationId.hasPrefix("g:") || m.isOutgoing ? Text("\(sender): ") : Text(""))
                    .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                    + Text(SearchText.highlighted(hit.snippet)).font(.system(size: 12))
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

struct ConversationRow: View {
    let conversation: Conversation
    let title: String
    let selected: Bool
    let line: ChatList.Line

    var body: some View {
        let unread = conversation.unreadCount > 0
        HStack(spacing: 12) {
            Avatar(id: conversation.id, name: title)
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(title).font(.system(size: 14, weight: unread ? .bold : .semibold)).lineLimit(1)
                    Spacer(minLength: 6)
                    // relative times ("Now", "5m", "Yesterday"…) move on by themselves
                    TimelineView(.everyMinute) { context in
                        Text(Formatting().listTime(conversation.lastMessageAt, now: context.date))
                            .font(.system(size: 12)).foregroundStyle(Theme.secondaryText)
                    }
                }
                HStack(spacing: 4) {
                    secondLine.font(.system(size: 13)).lineLimit(1)
                    Spacer(minLength: 6)
                    if unread {
                        Text("\(conversation.unreadCount)").font(.system(size: 11, weight: .bold))
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Theme.outgoing, in: Capsule())
                            .foregroundStyle(.white)
                    }
                }
                .foregroundStyle(unread ? .primary : Theme.secondaryText)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(selected ? Theme.selection : .clear, in: RoundedRectangle(cornerRadius: 10))
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder private var secondLine: some View {
        switch line {
        case .typing:
            Text("typing…").italic()
        case .draft(let text):
            Text("Draft: ").italic().bold() + Text(text)
        case .message(let text, let tick):
            if let tick { TickIcon(tick: tick) }
            Text(text).italic(conversation.lastMessageDeleted)
        }
    }
}
