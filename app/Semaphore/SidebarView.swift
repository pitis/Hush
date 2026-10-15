import SwiftUI

struct SidebarView: View {
    let store: AppStore
    @State private var query = ""
    @FocusState private var searchFocused: Bool

    private var shown: [Conversation] {
        store.conversations.filter { Formatting.matches(AppStore.displayTitle($0), query: query) }
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
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(Theme.secondaryText)
                TextField("Search", text: $query).textFieldStyle(.plain).focused($searchFocused)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(Theme.field, in: RoundedRectangle(cornerRadius: 8))
            .padding(.horizontal, 14)
            .padding(.bottom, 8)
            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(shown, id: \.id) { c in
                        ConversationRow(
                            conversation: c, selected: c.id == store.selectedId, typing: store.isTyping(c.id)
                        )
                        .contentShape(Rectangle())
                        .onTapGesture { store.select(c.id) }
                    }
                }
                .padding(.horizontal, 8)
            }
        }
        .background(Theme.sidebar)
        .background {
            Button("") { searchFocused = true }.keyboardShortcut("f", modifiers: .command).hidden()
        }
    }
}

struct ConversationRow: View {
    let conversation: Conversation
    let selected: Bool
    let typing: Bool

    var body: some View {
        let title = AppStore.displayTitle(conversation)
        HStack(spacing: 12) {
            Avatar(id: conversation.id, name: title)
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(title).font(.system(size: 14, weight: conversation.unreadCount > 0 ? .bold : .semibold))
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    Text(Formatting().listTime(conversation.lastMessageAt)).font(.system(size: 12))
                        .foregroundStyle(Theme.secondaryText)
                }
                HStack {
                    Text(typing ? "typing…" : Formatting.preview(conversation))
                        .font(.system(size: 13))
                        .foregroundStyle(conversation.unreadCount > 0 ? .primary : Theme.secondaryText)
                        .italic(typing)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    if conversation.unreadCount > 0 {
                        Text("\(conversation.unreadCount)").font(.system(size: 11, weight: .bold))
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Theme.outgoing, in: Capsule())
                            .foregroundStyle(.white)
                    }
                }
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(selected ? Theme.selection : .clear, in: RoundedRectangle(cornerRadius: 10))
        .accessibilityElement(children: .combine)
    }
}
