import SwiftUI

struct SidebarView: View {
    @Bindable var store: AppStore
    @FocusState private var searchFocused: Bool
    @FocusState private var listFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Chats").font(.system(size: 22, weight: .bold))
                Spacer()
            }
            .padding(.horizontal, 18)
            .padding(.top, 14)
            .padding(.bottom, 10)
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(Theme.secondaryText)
                TextField("Search", text: $store.searchQuery).textFieldStyle(.plain).focused($searchFocused)
                    // like Desktop's search: arrows pick a chat, ↩ opens it, Esc ends the search
                    .onKeyPress(.downArrow) { move(1) }
                    .onKeyPress(.upArrow) { move(-1) }
                    .onKeyPress(.return) {
                        store.openSearchResult()
                        return .handled
                    }
                    .onExitCommand {
                        store.searchQuery = ""
                        searchFocused = false
                    }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(Theme.field, in: RoundedRectangle(cornerRadius: 8))
            .padding(.horizontal, 14)
            .padding(.bottom, 8)
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 2) {
                        ForEach(store.visibleConversations, id: \.id) { c in
                            let selected = c.id == store.selectedId
                            Button {
                                store.select(c.id)
                                store.focusComposer()
                            } label: {
                                ConversationRow(
                                    conversation: c, title: store.title(c), selected: selected,
                                    line: ChatList.line(
                                        c, selected: selected, typing: store.isTyping(c.id),
                                        draft: Composer.draft(for: c.id),
                                        authorName: store.senderName(c.lastMessageAuthor))
                                )
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .accessibilityAddTraits(selected ? .isSelected : [])
                            .id(c.id)
                        }
                    }
                    .padding(.horizontal, 8)
                }
                .focusable()
                .focused($listFocused)
                .focusEffectDisabled()
                .onKeyPress(.downArrow) { move(1) }
                .onKeyPress(.upArrow) { move(-1) }
                .onChange(of: store.selectedId) { _, id in
                    // keyboard navigation can select a chat that is scrolled out of view
                    if let id { proxy.scrollTo(id) }
                }
            }
        }
        .background(Theme.sidebar)
        .onChange(of: store.searchFocusRequest) { _, _ in searchFocused = true }
    }

    private func move(_ step: Int) -> KeyPress.Result {
        store.selectNeighbor(step: step, unreadOnly: false)
        return .handled
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
