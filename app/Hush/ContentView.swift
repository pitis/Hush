import SwiftUI

struct ContentView: View {
    @AppStorage(Appearance.key) private var appearance = Appearance.default
    let core: AppStore
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Group {
            switch core.screen {
            case .starting:
                ProgressView()
            case .failed(let message):
                ContentUnavailableView(
                    "Hush couldn't start", systemImage: "exclamationmark.octagon", description: Text(message))
            case .link:
                LinkView(url: core.linkURL, error: core.lastError, retry: core.retryLink)
            case .unlinked:
                UnlinkedView(store: core)
            case .chats:
                ZStack {
                    NavigationSplitView {
                        VStack(spacing: 0) {
                            ConnectionBanner(store: core)
                            ErrorBanner(store: core)
                            SidebarView(store: core)
                        }
                        .navigationSplitViewColumnWidth(min: 300, ideal: 340, max: 460)
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
                                    core.conversations.isEmpty ? "New messages appear here as they arrive." : ""))
                        }
                    }
                    .navigationTitle("")
                    if let progress = core.history {
                        Color.black.opacity(0.35).ignoresSafeArea()
                        HistoryView(progress: progress, cancel: core.cancelHistory, retry: core.retryHistory)
                    }
                }
            }
        }
        .frame(minWidth: 820, minHeight: 520)
        .background(Theme.window)
        .onChange(of: appearance, initial: true) { _, chosen in chosen.apply() }
        .task { await core.boot() }
        .onAppear { WindowOpener.open = openWindow }
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

/// Shown over the chat list while not connected, like Desktop's: what is going on, and Retry.
struct ConnectionBanner: View {
    let store: AppStore

    var body: some View {
        switch store.connection {
        case .online, .unlinked:
            EmptyView()
        case .connecting:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Connecting…").font(.system(size: 12))
                Spacer()
            }
            .padding(10)
            .background(Theme.selection)
        case .offline:
            HStack(spacing: 8) {
                Image(systemName: "wifi.slash")
                Text("Offline. Messages will arrive once you're connected.").font(.system(size: 12))
                Spacer(minLength: 4)
                Button("Retry", action: store.retryConnection).controlSize(.small)
            }
            .padding(10)
            .background(Color.orange.opacity(0.25))
        }
    }
}

/// The last thing that failed (a pin, a mute, marking a chat read, a core error), over the chat
/// list until dismissed or until the next change works.
struct ErrorBanner: View {
    let store: AppStore

    var body: some View {
        if let error = store.lastError {
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                Text(error).font(.system(size: 12)).lineLimit(3)
                Spacer(minLength: 4)
                Button(action: store.dismissError) { Image(systemName: "xmark") }
                    .buttonStyle(.plain).accessibilityLabel("Dismiss")
            }
            .padding(10)
            .background(Color.yellow.opacity(0.2))
        }
    }
}

/// This Mac was removed on the phone (or the account is gone): link again from nothing.
struct UnlinkedView: View {
    let store: AppStore
    @State private var working = false

    var body: some View {
        ContentUnavailableView {
            Label("This Mac is no longer linked", systemImage: "iphone.slash")
        } description: {
            VStack(spacing: 8) {
                Text(
                    "It was removed from your phone's linked devices. Link it again to keep using Hush here. "
                        + "Messages stored on this Mac are removed first.")
                if let error = store.lastError { Text(error).foregroundStyle(.orange) }
            }
        } actions: {
            Button {
                working = true
                Task {
                    await store.linkAgain()
                    working = false
                }
            } label: {
                Text("Link Again")
            }
            .buttonStyle(.borderedProminent)
            .disabled(working)
        }
    }
}

/// Window ▸ Core Log: what went wrong recently (ids and reasons, never message text).
struct CoreLogView: View {
    let store: AppStore

    var body: some View {
        Group {
            if store.errors.isEmpty {
                ContentUnavailableView("No errors", systemImage: "checkmark.circle")
            } else {
                List(store.errors.reversed()) { line in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(line.at, format: .dateTime.hour().minute().second()).monospacedDigit()
                            .foregroundStyle(.secondary)
                        Text(line.text).textSelection(.enabled)
                    }
                    .font(.system(size: 12))
                }
            }
        }
        .frame(minWidth: 520, minHeight: 300)
        .toolbar {
            Button("Copy All") {
                let text = store.errors.map { "\($0.at.formatted(.iso8601)) \($0.text)" }.joined(separator: "\n")
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            }
            .disabled(store.errors.isEmpty)
        }
    }
}
