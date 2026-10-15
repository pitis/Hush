import SwiftUI

struct ContentView: View {
    let core: CoreBridge

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
            case .chats where core.conversations.isEmpty:
                ContentUnavailableView(
                    core.connection == .online ? "Connected — no messages yet" : "Connecting…",
                    systemImage: "bubble.left.and.bubble.right",
                    description: Text(
                        core.lastError ?? "New messages appear here as they arrive. Older history isn't synced yet."))
            case .chats:
                HSplitView {
                    List(core.conversations, id: \.id) { conversation in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(conversation.title.isEmpty ? conversation.id : conversation.title).bold()
                                .lineLimit(1)
                            Text(conversation.lastMessagePreview).lineLimit(1).foregroundStyle(.secondary)
                        }
                        .badge(Int(conversation.unreadCount))
                    }
                    .frame(minWidth: 260)
                    List(core.log.reversed()) { Text($0.text).font(.caption.monospaced()) }
                        .frame(minWidth: 320)
                }
            }
        }
        .frame(minWidth: 720, minHeight: 480)
        .navigationTitle("Semaphore — \(String(describing: core.connection))")
        .task { await core.boot() }
    }
}
