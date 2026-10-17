import Foundation
import OSLog
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
    /// A non-message category (calls); wins over `hasActions`.
    var category: String?
}

enum Notifications {
    static let category = "message"
    static let replyAction = "reply"
    static let markReadAction = "mark-read"
    static let callCategory = "call"
    static let acceptCallAction = "accept-call"
    static let declineCallAction = "decline-call"

    /// The one ringing notification of a conversation's incoming call.
    static func callId(_ conversationId: String) -> String { "call|\(conversationId)" }

    /// Rings until answered, declined, or the caller gives up. Nil when notifications are off;
    /// the caller's name only when the setting shows names.
    static func incomingCall(setting: NotificationContent, conversationId: String, title: String)
        -> NotificationRequest?
    {
        guard let title = callTitle(setting, title) else { return nil }
        return NotificationRequest(
            id: callId(conversationId), conversationId: conversationId, title: title,
            body: "Incoming voice call", hasActions: false, category: callCategory)
    }

    /// Posted when an incoming call ends without ever connecting (same privacy rules).
    static func missedCall(setting: NotificationContent, conversationId: String, title: String, at: Date)
        -> NotificationRequest?
    {
        guard let title = callTitle(setting, title) else { return nil }
        return NotificationRequest(
            id: "call-missed|\(conversationId)|\(Int(at.timeIntervalSince1970))", conversationId: conversationId,
            title: title, body: "Missed voice call", hasActions: false)
    }

    private static func callTitle(_ setting: NotificationContent, _ name: String) -> String? {
        switch setting {
        case .off: nil
        case .noNameOrMessage: "Hush"
        case .nameOnly, .nameAndMessage: name
        }
    }

    /// The id of a notification about a message: the message itself (`reactor` nil) or a reaction
    /// to it. All of a message's notifications share `idPrefix`, to remove them together.
    static func id(_ conversationId: String, _ key: MessageKey, reactor: String?, emoji: String?) -> String {
        idPrefix(conversationId, key) + (reactor.map { "\($0)|\(emoji ?? "")" } ?? "")
    }

    static func idPrefix(_ conversationId: String, _ key: MessageKey) -> String {
        "\(conversationId)|\(key.author)|\(key.sentAt)|"
    }

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
            title = "Hush"
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
    func remove(ids: [String])
    /// Removes the delivered notifications whose ids start with `idPrefix`, including ones shown
    /// before the app last started.
    func remove(idPrefix: String)
    func removeAll()
}

/// Notification Center. Notifications of a chat share its thread, so they group together.
final class SystemNotifier: Notifier {
    private var center: UNUserNotificationCenter { .current() }
    /// Ids and errors only, never what a notification says.
    private static let logger = Logger(subsystem: "dev.pitis.hush", category: "notifications")

    func authorize() {
        let reply = UNTextInputNotificationAction(
            identifier: Notifications.replyAction, title: "Reply", textInputButtonTitle: "Send",
            textInputPlaceholder: "Message")
        let markRead = UNNotificationAction(identifier: Notifications.markReadAction, title: "Mark as Read")
        let accept = UNNotificationAction(
            identifier: Notifications.acceptCallAction, title: "Accept", options: [.foreground])
        let decline = UNNotificationAction(
            identifier: Notifications.declineCallAction, title: "Decline", options: [.destructive])
        center.setNotificationCategories([
            UNNotificationCategory(
                identifier: Notifications.category, actions: [reply, markRead], intentIdentifiers: []),
            UNNotificationCategory(
                identifier: Notifications.callCategory, actions: [accept, decline], intentIdentifiers: []),
        ])
        center.requestAuthorization(options: [.alert, .sound]) { granted, error in
            if let error {
                Self.logger.error("notification permission failed: \(error, privacy: .public)")
            } else if !granted {
                Self.logger.notice("notifications are turned off for Hush in System Settings")
            }
        }
    }

    func post(_ request: NotificationRequest) {
        let content = UNMutableNotificationContent()
        content.title = request.title
        content.body = request.body
        content.sound = .default
        content.threadIdentifier = request.conversationId
        content.userInfo = ["conversationId": request.conversationId]
        if let category = request.category {
            content.categoryIdentifier = category
        } else if request.hasActions {
            content.categoryIdentifier = Notifications.category
        }
        center.add(UNNotificationRequest(identifier: request.id, content: content, trigger: nil)) { error in
            if let error { Self.logger.error("showing a notification failed: \(error, privacy: .public)") }
        }
    }

    func remove(conversationId: String) {
        center.getDeliveredNotifications { delivered in
            let ids = delivered.filter { $0.request.content.threadIdentifier == conversationId }
                .map(\.request.identifier)
            UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ids)
        }
    }

    func remove(ids: [String]) { center.removeDeliveredNotifications(withIdentifiers: ids) }

    func remove(idPrefix: String) {
        center.getDeliveredNotifications { delivered in
            let ids = delivered.map(\.request.identifier).filter { $0.hasPrefix(idPrefix) }
            UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ids)
        }
    }

    func removeAll() { center.removeAllDeliveredNotifications() }
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
        case Notifications.acceptCallAction:
            await store.acceptCall()
        case Notifications.declineCallAction:
            await store.hangupCall()
        default:
            await store.openFromNotification(conversationId: id)
        }
    }

    /// The app decides when to post (not while its window is in front); what it posts is shown,
    /// also while the app is active with its window closed or minimized.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async
        -> UNNotificationPresentationOptions
    { [.banner, .list, .sound] }
}
