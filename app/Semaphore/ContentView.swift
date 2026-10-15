import SwiftUI

struct ContentView: View {
    let core: AppStore

    var body: some View {
        Group {
            switch core.screen {
            case .starting:
                ProgressView()
            case .failed(let message):
                ContentUnavailableView(
                    "Semaphore couldn't start", systemImage: "exclamationmark.octagon", description: Text(message))
            case .link:
                LinkView(url: core.linkURL, error: core.lastError, retry: core.retryLink)
            case .chats:
                NavigationSplitView {
                    SidebarView(store: core).navigationSplitViewColumnWidth(min: 280, ideal: 340, max: 460)
                } detail: {
                    if let c = core.selectedConversation {
                        ConversationView(store: core, conversation: c)
                    } else {
                        ContentUnavailableView(
                            core.conversations.isEmpty
                                ? (core.connection == .online ? "Connected — no messages yet" : "Connecting…")
                                : "Select a chat",
                            systemImage: "bubble.left.and.bubble.right",
                            description: Text(
                                core.lastError
                                    ?? (core.conversations.isEmpty
                                        ? "New messages appear here as they arrive. Older history isn't synced yet."
                                        : "")))
                    }
                }
                .navigationTitle("")
            }
        }
        .frame(minWidth: 820, minHeight: 520)
        .background(Theme.window)
        .preferredColorScheme(.dark)
        .task { await core.boot() }
    }
}
