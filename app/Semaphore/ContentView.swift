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
                    SidebarView(store: core).navigationSplitViewColumnWidth(min: 300, ideal: 340, max: 460)
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
        .task {
            // "Today" separators become "Yesterday" at midnight
            var day = Calendar.current.startOfDay(for: .now)
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(60))
                let today = Calendar.current.startOfDay(for: .now)
                if today != day {
                    day = today
                    core.refreshDays()
                }
            }
        }
    }
}
