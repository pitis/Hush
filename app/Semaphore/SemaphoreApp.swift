import ServiceManagement
import SwiftUI
import UserNotifications

/// Closing the window keeps Semaphore running (notifications, the menu bar item), as Signal
/// Desktop does; ⌘Q quits.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

@main
struct SemaphoreApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    /// XCTest launches the app as its test host; it must not touch real data or the network then.
    private let isTestHost: Bool
    /// One core per app (it locks the data folder), owned by the app so closing the window doesn't
    /// tear it down.
    @State private var core: AppStore
    /// Kept for the app's lifetime: Notification Center holds its delegate weakly.
    private let notificationDelegate: NotificationDelegate?
    /// Reconnects as soon as the network comes back.
    private let networkWatch: NetworkWatch?
    /// The menu bar item with the unread count (an AppKit status item: a SwiftUI MenuBarExtra
    /// can't be left out of the XCTest host, and it hangs XCTest there).
    private let menuBar: MenuBarItem?

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
        menuBar = isTestHost ? nil : MenuBarItem(store: core)
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
    @AppStorage(MenuBar.key) private var showInMenuBar = true
    @State private var openAtLogin = LoginItem.isEnabled
    @State private var loginItemError: String?
    @State private var confirmUnlink = false
    @State private var unlinkFailed: String?
    @State private var working = false

    var body: some View {
        Form {
            Picker("Notification content", selection: $content) {
                ForEach(NotificationContent.allCases) { Text($0.label).tag($0) }
            }
            .pickerStyle(.radioGroup)
            Toggle("Open at login", isOn: $openAtLogin)
                .onChange(of: openAtLogin) { _, on in
                    // putting the toggle back after a failure changes it again: nothing to do then
                    guard on != LoginItem.isEnabled else { return }
                    do {
                        try LoginItem.set(on)
                        loginItemError =
                            LoginItem.needsApproval
                            ? "Allow Semaphore in System Settings ▸ General ▸ Login Items." : nil
                    } catch {
                        loginItemError = "Couldn't change the login item: \(error.localizedDescription)"
                        openAtLogin = LoginItem.isEnabled
                    }
                }
            Toggle("Show in menu bar", isOn: $showInMenuBar)
            if let loginItemError { Text(loginItemError).font(.caption).foregroundStyle(.secondary) }
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

/// Opening Semaphore at login (System Settings ▸ General ▸ Login Items lists it).
enum LoginItem {
    static var isEnabled: Bool { SMAppService.mainApp.status == .enabled }
    /// Registered, but macOS waits for the user to allow it.
    static var needsApproval: Bool { SMAppService.mainApp.status == .requiresApproval }

    static func set(_ on: Bool) throws {
        if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
    }
}

enum MenuBar {
    static let key = "menuBar.show"
    static let unreadChatsShown = 5
}
