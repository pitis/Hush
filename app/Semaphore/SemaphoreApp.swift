import SwiftUI
import UserNotifications

@main
struct SemaphoreApp: App {
    /// XCTest launches the app as its test host; it must not touch real data or the network then.
    private let isTestHost: Bool
    /// One core per app (it locks the data folder), owned by the app so closing the window doesn't
    /// tear it down.
    @State private var core: AppStore
    /// Kept for the app's lifetime: Notification Center holds its delegate weakly.
    private let notificationDelegate: NotificationDelegate?
    /// Reconnects as soon as the network comes back.
    private let networkWatch: NetworkWatch?

    init() {
        let isTestHost = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
        let core = AppStore(notifier: isTestHost ? nil : SystemNotifier())
        if !isTestHost { Files.clearOpened() }
        self.isTestHost = isTestHost
        _core = State(initialValue: core)
        // set before launch finishes, so a click on a notification that launched the app arrives
        notificationDelegate = isTestHost ? nil : NotificationDelegate(store: core)
        if let notificationDelegate { UNUserNotificationCenter.current().delegate = notificationDelegate }
        networkWatch = isTestHost ? nil : NetworkWatch { core.networkReturned() }
    }

    var body: some Scene {
        Window("Semaphore", id: "main") {
            if isTestHost { EmptyView() } else { ContentView(core: core) }
        }
        .commands { ChatCommands(store: core) }
        Window("Core Log", id: "core-log") { CoreLogView(store: core) }
        Settings { SettingsView(store: core) }
    }
}

/// Preferences (⌘,).
struct SettingsView: View {
    let store: AppStore
    @AppStorage(NotificationContent.key) private var content = NotificationContent.nameAndMessage
    @State private var confirmUnlink = false
    @State private var unlinkFailed: String?
    @State private var working = false

    var body: some View {
        Form {
            Picker("Notification content", selection: $content) {
                ForEach(NotificationContent.allCases) { Text($0.label).tag($0) }
            }
            .pickerStyle(.radioGroup)
            if store.screen == .chats {
                Divider()
                LabeledContent("Linked device") {
                    Button("Unlink This Mac…", role: .destructive) { confirmUnlink = true }.disabled(working)
                }
            }
        }
        .padding(20)
        .frame(width: 440)
        .confirmationDialog("Unlink this Mac?", isPresented: $confirmUnlink) {
            Button("Unlink", role: .destructive) { unlink(tellServer: true) }
        } message: {
            Text("Your messages and files are removed from this Mac. Your phone keeps them.")
        }
        .alert(
            "The server couldn't be reached", isPresented: .constant(unlinkFailed != nil),
            presenting: unlinkFailed
        ) { _ in
            Button("Unlink Anyway", role: .destructive) { unlink(tellServer: false) }
            Button("Cancel", role: .cancel) { unlinkFailed = nil }
        } message: { reason in
            Text(
                "\(reason)\n\nUnlinking anyway removes everything here, but your phone keeps listing this Mac "
                    + "until you remove it there.")
        }
    }

    private func unlink(tellServer: Bool) {
        unlinkFailed = nil
        working = true
        Task {
            let failed = await store.unlinkThisMac(tellServer: tellServer)
            working = false
            unlinkFailed = tellServer ? failed : nil
            if !tellServer, let failed { store.showError(failed) }
        }
    }
}

/// Signal Desktop's navigation shortcuts, plus ⌘K for the chat search.
struct ChatCommands: Commands {
    let store: AppStore

    /// Chat shortcuts work from anywhere and land in the new chat's composer, like Desktop.
    private func switchChat(_ action: () -> Void) {
        action()
        store.focusComposer()
    }

    var body: some Commands {
        CommandMenu("Chats") {
            Group {
                Button("Find Chat…") { store.focusSearch() }.keyboardShortcut("k")
                Button("Search") { store.focusSearch() }.keyboardShortcut("f")
                Button("Search in Chat") { store.searchInChat() }.keyboardShortcut("f", modifiers: [.command, .shift])
                    .disabled(store.selectedId == nil)
                Divider()
                Button("Previous Chat") { switchChat { store.selectNeighbor(step: -1, unreadOnly: false) } }
                    .keyboardShortcut(.upArrow, modifiers: .option)
                Button("Next Chat") { switchChat { store.selectNeighbor(step: 1, unreadOnly: false) } }
                    .keyboardShortcut(.downArrow, modifiers: .option)
                Button("Previous Unread Chat") { switchChat { store.selectNeighbor(step: -1, unreadOnly: true) } }
                    .keyboardShortcut(.upArrow, modifiers: [.option, .shift])
                Button("Next Unread Chat") { switchChat { store.selectNeighbor(step: 1, unreadOnly: true) } }
                    .keyboardShortcut(.downArrow, modifiers: [.option, .shift])
                Menu("Jump to Chat") {
                    ForEach(1...9, id: \.self) { n in
                        Button("Chat \(n)") { switchChat { store.selectChat(at: n) } }
                            .keyboardShortcut(KeyEquivalent(Character("\(n)")))
                    }
                }
                Divider()
                Button("Focus Composer") { store.focusComposer() }.keyboardShortcut("t", modifiers: [.command, .shift])
                    .disabled(store.selectedId == nil)
                Button("Close Chat") { store.select(nil) }.keyboardShortcut("c", modifiers: [.command, .shift])
                    .disabled(store.selectedId == nil)
            }
            .disabled(store.screen != .chats)
        }
    }
}
