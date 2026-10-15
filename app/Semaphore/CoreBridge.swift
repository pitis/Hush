import Foundation
import OSLog
import Observation

/// App-side state fed by core events. Plan 2 grows this into the full app store.
@MainActor @Observable
final class CoreBridge {
    enum Screen: Equatable {
        case starting, link, chats
        case failed(String)
    }

    struct LogLine: Identifiable, Equatable {
        let id: Int
        let text: String
    }

    /// Opens the core; injectable so tests can use a fake client.
    typealias Opener = @Sendable (EventListener) throws -> any ClientProtocol

    static let logLimit = 200
    static let deviceName = "Semaphore"
    /// App state for Console.app (subsystem dev.pitis.semaphore). Never URLs or message content.
    private static let logger = Logger(subsystem: "dev.pitis.semaphore", category: "app")

    private(set) var screen: Screen = .starting
    private(set) var linkURL: String?
    private(set) var connection: ConnectionState = .offline
    private(set) var conversations: [Conversation] = []
    private(set) var log: [LogLine] = []
    private(set) var lastError: String?

    private let open: Opener
    private var client: (any ClientProtocol)?
    private var booted = false
    private var nextLogId = 0
    private var refreshing = false
    private var refreshAgain = false

    init(open: @escaping Opener = CoreBridge.openReal) {
        self.open = open
    }

    /// `~/Library/Application Support/Semaphore`. Changing it orphans the user's data.
    nonisolated static func dataDirectory() throws -> URL {
        try FileManager.default
            .url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("Semaphore", isDirectory: true)
    }

    nonisolated static func openReal(listener: EventListener) throws -> any ClientProtocol {
        try Client.open(dataDir: dataDirectory().path, dbKeyHex: Keychain().dbKey(), listener: listener)
    }

    /// Opens the core (off the main thread), then either starts receiving or starts linking. Runs once.
    func boot() async {
        guard !booted else { return }
        booted = true
        let listener = Listener(bridge: self)
        let open = self.open
        let opened = await Task.detached { Result { try open(listener) } }.value
        do {
            let client = try opened.get()
            self.client = client
            if client.isLinked() {
                screen = .chats
                try client.start()
                refreshConversations()
            } else {
                screen = .link
                try client.link(deviceName: Self.deviceName)
            }
        } catch {
            Self.logger.error("boot failed: \(Self.describe(error), privacy: .public)")
            screen = .failed(Self.describe(error))
        }
    }

    /// Asks the core for a new linking QR code after a failed or expired attempt.
    func retryLink() {
        guard let client, linkURL == nil else { return }
        lastError = nil
        do { try client.link(deviceName: Self.deviceName) } catch { lastError = Self.describe(error) }
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .linkUrl(let url):
            Self.logger.notice("link code ready")
            linkURL = url
            lastError = nil
            screen = .link
        case .linked:
            Self.logger.notice("linked")
            linkURL = nil
            lastError = nil
            screen = .chats
            do { try client?.start() } catch { append("start failed: \(error)") }
        case .connection(let state):
            Self.logger.notice("connection \(String(describing: state), privacy: .public)")
            connection = state
            if state == .unlinked {
                linkURL = nil
                screen = .link
            } else if client?.isLinked() == true, screen != .chats {
                // only a linked core moves us to the chats (a core failure mid-link must not)
                screen = .chats
            }
        case .ready, .conversationsChanged:
            refreshConversations()
        case .messageChanged(let conversationId, let key):
            append("message \(conversationId) \(key.author.prefix(8)) @\(key.sentAt)")
            refreshConversations()
        case .typing(let conversationId, _, let started):
            append("typing \(conversationId) \(started ? "started" : "stopped")")
        case .error(let message):
            // error text can mention ids or numbers: keep it out of the system log
            Self.logger.error("core error: \(message, privacy: .private)")
            lastError = message
            append("error: \(message)")
        }
    }

    /// One query at a time; events arriving meanwhile cause exactly one more pass, so a burst
    /// can't flood the core and an older result can never overwrite a newer one.
    private func refreshConversations() {
        guard let client else { return }
        if refreshing {
            refreshAgain = true
            return
        }
        refreshing = true
        Task {
            repeat {
                refreshAgain = false
                do { conversations = try await client.conversations() } catch { append("query failed: \(error)") }
            } while refreshAgain
            refreshing = false
        }
    }

    private func append(_ text: String) {
        log.append(LogLine(id: nextLogId, text: text))
        nextLogId += 1
        if log.count > Self.logLimit { log.removeFirst(log.count - Self.logLimit) }
    }

    private static func describe(_ error: Error) -> String {
        switch error {
        case CoreError.AlreadyOpen:
            return "Semaphore is already running."
        case KeychainError.status(errSecInteractionNotAllowed), KeychainError.status(errSecAuthFailed),
            KeychainError.status(errSecUserCanceled):
            return "Semaphore couldn't read its key from your keychain. Unlock the keychain, allow access, and reopen."
        case CoreError.Storage(let message):
            return "Semaphore couldn't open its data: \(message)"
        default:
            return "\(error)"
        }
    }
}

/// Receives core events on the core thread and hands them to the bridge on the main actor.
final class Listener: EventListener {
    private let deliver: @Sendable (CoreEvent) -> Void

    init(bridge: CoreBridge) {
        deliver = { [weak bridge] event in
            Task { @MainActor in bridge?.handle(event) }
        }
    }

    func onEvent(event: CoreEvent) { deliver(event) }
}
