import SwiftUI

struct ContentView: View {
    @State private var core = CoreBridge()

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
                    List(core.log.reversed(), id: \.self) { Text($0).font(.caption.monospaced()) }
                        .frame(minWidth: 320)
                }
            }
        }
        .frame(minWidth: 720, minHeight: 480)
        .navigationTitle("Semaphore — \(String(describing: core.connection))")
        .task { core.boot() }
    }
}
