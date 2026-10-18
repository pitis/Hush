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
                LinkView(
                    url: core.linkURL, error: core.lastError, note: core.restoreNote, retry: core.retryLink,
                    restore: { await core.restoreBackup(from: $0, passphrase: $1) })
            case .unlinked:
                UnlinkedView(store: core)
            case .chats:
                ZStack {
                    NavigationSplitView {
                        SidebarView(store: core)
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
                    // a group link clicked in a message joins here, not in the browser
                    .environment(\.openURL, OpenURLAction { core.handleLink($0) ? .handled : .systemAction })
                    .sheet(
                        isPresented: Binding(
                            get: { core.joiningLink != nil }, set: { if !$0 { core.joiningLink = nil } })
                    ) {
                        JoinGroupSheet(store: core, link: core.joiningLink ?? "")
                    }
                    if let progress = core.history {
                        Color.black.opacity(0.35).ignoresSafeArea()
                        HistoryView(progress: progress, cancel: core.cancelHistory, retry: core.retryHistory)
                    }
                }
            }
        }
        .frame(minWidth: 820, minHeight: 520)
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

/// One quiet line at the foot of the chat list while not connected (as Mail): what is going on,
/// and Retry.
struct ConnectionStatus: View {
    let store: AppStore

    var body: some View {
        switch store.connection {
        case .online, .unlinked:
            EmptyView()
        case .connecting:
            HStack(spacing: 6) {
                ProgressView().controlSize(.mini)
                Text("Connecting…")
                Spacer()
            }
            .font(.callout).foregroundStyle(.secondary)
            .padding(.horizontal, 16).padding(.vertical, 8)
        case .offline:
            HStack(spacing: 6) {
                Image(systemName: "wifi.slash")
                Text("Offline")
                Spacer(minLength: 4)
                Button("Retry", action: store.retryConnection).buttonStyle(.link)
            }
            .font(.callout).foregroundStyle(.secondary)
            .help("Messages will arrive once you're connected.")
            .padding(.horizontal, 16).padding(.vertical, 8)
        }
    }
}

/// The last thing that failed (a pin, a mute, marking a chat read, a core error), at the top of
/// the chat list until dismissed or until the next change works.
struct ErrorBanner: View {
    let store: AppStore

    var body: some View {
        if let error = store.lastError {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow)
                Text(error).font(.callout).lineLimit(3)
                Spacer(minLength: 4)
                Button(action: store.dismissError) { Image(systemName: "xmark") }
                    .buttonStyle(.borderless).accessibilityLabel("Dismiss")
            }
            .padding(10)
            // in a shape: never a fill reaching into the title bar
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .padding(.horizontal, 10)
            .padding(.bottom, 6)
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
