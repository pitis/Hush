import AppKit
import SwiftUI

/// The menu bar item: the unread count, and a menu to open the window, jump to a chat with
/// unread messages, or quit. Settings ▸ Show in menu bar hides it.
@MainActor
final class MenuBarItem: NSObject, NSMenuDelegate {
    private let store: AppStore
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private var defaultsObserver: NSObjectProtocol?

    init(store: AppStore) {
        self.store = store
        super.init()
        let menu = NSMenu()
        menu.delegate = self
        item.menu = menu
        update()
        observeStore()
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in MainActor.assumeIsolated { self?.update() } }
    }

    private func update() {
        item.isVisible = UserDefaults.standard.object(forKey: MenuBar.key) as? Bool ?? true
        let unread = store.badge
        item.button?.image = NSImage(
            systemSymbolName: unread == nil ? "bubble.left.and.bubble.right" : "bubble.left.and.bubble.right.fill",
            accessibilityDescription: "Hush")
        item.button?.title = unread.map { " \($0)" } ?? ""
        item.button?.imagePosition = .imageLeading
        item.button?.setAccessibilityLabel(unread.map { "Hush, \($0) unread" } ?? "Hush")
    }

    /// Re-renders when the unread count changes (Observation: re-registers after each change).
    private func observeStore() {
        withObservationTracking {
            _ = store.badge
        } onChange: { [weak self] in
            Task { @MainActor in
                self?.update()
                self?.observeStore()
            }
        }
    }

    // The menu is built when it opens, so it always lists the current unread chats.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        menu.addItem(action("Open Hush") { [weak self] in self?.show(nil) })
        let unread = ChatList.unreadChats(store.conversations, now: Date(), limit: MenuBar.unreadChatsShown)
        if !unread.isEmpty {
            menu.addItem(.separator())
            for c in unread {
                let id = c.id
                menu.addItem(action(ChatList.menuTitle(store.title(c), c)) { [weak self] in self?.show(id) })
            }
        }
        menu.addItem(.separator())
        let quit = action("Quit Hush") { NSApp.terminate(nil) }
        quit.keyEquivalent = "q"
        menu.addItem(quit)
    }

    private func show(_ conversationId: String?) {
        WindowOpener.openMain()
        NSApp.activate()
        if let conversationId {
            store.select(conversationId)
            store.focusComposer()
        }
    }

    private func action(_ title: String, _ perform: @escaping @MainActor () -> Void) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: #selector(MenuAction.run), keyEquivalent: "")
        let target = MenuAction(perform)
        item.target = target
        item.representedObject = target  // the item keeps its target alive
        return item
    }
}

/// An NSMenuItem target running a closure.
@MainActor
private final class MenuAction: NSObject {
    private let perform: @MainActor () -> Void
    init(_ perform: @escaping @MainActor () -> Void) { self.perform = perform }
    @objc func run() { perform() }
}

/// Opens the main window from outside SwiftUI (the menu bar item); the window registers how.
@MainActor
enum WindowOpener {
    static var open: OpenWindowAction?

    static func openMain() {
        if let window = NSApp.windows.first(where: { $0.identifier?.rawValue == "main" }) {
            window.makeKeyAndOrderFront(nil)
        } else {
            open?(id: "main")
        }
    }
}
