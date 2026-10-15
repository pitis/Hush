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

    static let logLimit = 200
    /// App state for Console.app (subsystem dev.pitis.semaphore). Never URLs or message content.
    private static let logger = Logger(subsystem: "dev.pitis.semaphore", category: "app")

    private(set) var screen: Screen = .starting
    private(set) var linkURL: String?
    private(set) var connection: ConnectionState = .offline
    private(set) var conversations: [Conversation] = []
    private(set) var log: [String] = []
    private(set) var lastError: String?
    private var client: Client?

    /// Opens the core (off the main thread) with the Keychain key, then either starts receiving
    /// or starts linking.
    func boot() async {
        let listener = Listener(bridge: self)
        let opened = await Task.detached { () -> Result<Client, Error> in
            Result {
                let dir = try FileManager.default
                    .url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
                    .appendingPathComponent("Semaphore", isDirectory: true)
                return try Client.open(dataDir: dir.path, dbKeyHex: try Keychain().dbKey(), listener: listener)
            }
        }.value
        do {
            let client = try opened.get()
            self.client = client
            if client.isLinked() {
                screen = .chats
                try client.start()
                refreshConversations()
            } else {
                screen = .link
                try client.link(deviceName: "Semaphore")
            }
        } catch CoreError.AlreadyOpen {
            Self.logger.error("already running")
            screen = .failed("Semaphore is already running.")
        } catch {
            screen = .failed("\(error)")
        }
    }

    /// Asks the core for a new linking QR code after a failed attempt.
    func retryLink() {
        lastError = nil
        do { try client?.link(deviceName: "Semaphore") } catch { lastError = "\(error)" }
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .linkUrl(let url):
            Self.logger.notice("link code ready")
            linkURL = url
            screen = .link
        case .linked:
            Self.logger.notice("linked")
            linkURL = nil
            screen = .chats
            do { try client?.start() } catch { append("start failed: \(error)") }
        case .connection(let state):
            Self.logger.notice("connection \(String(describing: state), privacy: .public)")
            connection = state
            if state == .unlinked {
                linkURL = nil
                screen = .link
            } else {
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
            Self.logger.error("core error: \(message, privacy: .public)")
            lastError = message
            append("error: \(message)")
        }
    }

    private func refreshConversations() {
        guard let client else { return }
        Task {
            do { conversations = try await client.conversations() } catch { append("query failed: \(error)") }
        }
    }

    private func append(_ line: String) {
        log.append(line)
        if log.count > Self.logLimit { log.removeFirst(log.count - Self.logLimit) }
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
