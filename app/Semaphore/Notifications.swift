import Foundation
import UserNotifications

/// What notifications show: Signal Desktop's "Notification content" setting (same stored values).
enum NotificationContent: String, CaseIterable, Identifiable {
    case nameAndMessage = "message"
    case nameOnly = "name"
    case noNameOrMessage = "count"
    case off

    static let key = "notifications.content"
    nonisolated(unsafe) static var defaults: UserDefaults = .standard  // swapped only by tests

    var id: String { rawValue }

    var label: String {
        switch self {
        case .nameAndMessage: "Name, content, and actions"
        case .nameOnly: "Name only"
        case .noNameOrMessage: "No name or content"
        case .off: "Off"
        }
    }

    static func current(_ defaults: UserDefaults = defaults) -> NotificationContent {
        defaults.string(forKey: key).flatMap(NotificationContent.init(rawValue:)) ?? .nameAndMessage
    }
}

/// One notification to show.
struct NotificationRequest: Equatable {
    let id: String
    let conversationId: String
    let title: String
    let body: String
    /// Reply and Mark as Read (only when the content is shown).
    let hasActions: Bool
}

enum Notifications {
    static let category = "message"
    static let replyAction = "reply"
    static let markReadAction = "mark-read"

    /// The notification for a message (or a reaction to ours, `reaction` = its emoji) worded like
    /// Signal Desktop's, or nil when notifications are off.
    static func request(
        setting: NotificationContent, conversationId: String, id: String, sender: String, groupTitle: String?,
        text: String?, reaction: String?
    ) -> NotificationRequest? {
        let title: String
        let body: String
        switch setting {
        case .off:
            return nil
        case .noNameOrMessage:
            title = "Semaphore"
            body = "New Message"
        case .nameOnly, .nameAndMessage:
            title = groupTitle.map { "\(sender) in \($0)" } ?? sender
            let message = text ?? "Attachment"
            if setting == .nameOnly {
                body = "New Message"
            } else if let reaction {
                body = "\(sender) reacted \(reaction) to: \(message)"
            } else {
                body = message
            }
        }
        return NotificationRequest(
            id: id, conversationId: conversationId, title: title, body: body,
            hasActions: setting == .nameAndMessage)
    }
}

/// Shows and removes notifications; faked in tests.
protocol Notifier: AnyObject {
    func authorize()
    func post(_ request: NotificationRequest)
    func remove(conversationId: String)
}

/// Notification Center. Notifications of a chat share its thread, so they group together.
final class SystemNotifier: Notifier {
    private var center: UNUserNotificationCenter { .current() }

    func authorize() {
        let reply = UNTextInputNotificationAction(
            identifier: Notifications.replyAction, title: "Reply", textInputButtonTitle: "Send",
            textInputPlaceholder: "Message")
        let markRead = UNNotificationAction(identifier: Notifications.markReadAction, title: "Mark as Read")
        center.setNotificationCategories([
            UNNotificationCategory(
                identifier: Notifications.category, actions: [reply, markRead], intentIdentifiers: [])
        ])
        center.requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    func post(_ request: NotificationRequest) {
        let content = UNMutableNotificationContent()
        content.title = request.title
        content.body = request.body
        content.sound = .default
        content.threadIdentifier = request.conversationId
        content.userInfo = ["conversationId": request.conversationId]
        if request.hasActions { content.categoryIdentifier = Notifications.category }
        center.add(UNNotificationRequest(identifier: request.id, content: content, trigger: nil))
    }

    func remove(conversationId: String) {
        let center = self.center
        center.getDeliveredNotifications { delivered in
            let ids = delivered.filter { $0.request.content.threadIdentifier == conversationId }
                .map(\.request.identifier)
            center.removeDeliveredNotifications(withIdentifiers: ids)
        }
    }
}

/// Routes clicks and actions on notifications to the store.
final class NotificationDelegate: NSObject, UNUserNotificationCenterDelegate {
    private let store: AppStore

    init(store: AppStore) { self.store = store }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard let id = response.notification.request.content.userInfo["conversationId"] as? String else { return }
        let text = (response as? UNTextInputNotificationResponse)?.userText
        switch response.actionIdentifier {
        case Notifications.replyAction:
            if let text { await store.replyFromNotification(conversationId: id, text: text) }
        case Notifications.markReadAction:
            await store.markReadFromNotification(conversationId: id)
        default:
            await store.openFromNotification(conversationId: id)
        }
    }

    /// The app only posts while in the background; if one arrives in front anyway, show nothing.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async
        -> UNNotificationPresentationOptions
    { [] }
}
