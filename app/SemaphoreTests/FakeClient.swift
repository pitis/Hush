import Foundation
import XCTest

@testable import Semaphore

/// Records what the bridge asks of the core and returns scripted answers.
final class FakeClient: ClientProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var _linked: Bool
    private(set) var linkCalls: [String] = []
    private(set) var startCalls = 0
    private(set) var conversationCalls = 0
    var linkError: Error?
    var startError: Error?
    /// Each `conversations()` call takes the next answer; the last one repeats.
    var conversationAnswers: [Result<[Conversation], Error>] = [.success([])]
    /// When set, `conversations()` call number `n` (1-based) waits for `release` first.
    var holdCall: Int?
    private var held: CheckedContinuation<Void, Never>?

    init(linked: Bool) { _linked = linked }

    var isHolding: Bool { lock.withLock { held != nil } }

    func release() {
        lock.lock()
        let c = held
        held = nil
        lock.unlock()
        c?.resume()
    }

    func isLinked() -> Bool { lock.withLock { _linked } }
    func setLinked(_ v: Bool) { lock.withLock { _linked = v } }

    func link(deviceName: String) throws {
        lock.withLock { linkCalls.append(deviceName) }
        if let linkError { throw linkError }
    }

    func start() throws {
        lock.withLock { startCalls += 1 }
        if let startError { throw startError }
    }

    func conversations() async throws -> [Conversation] {
        let n = lock.withLock { () -> Int in
            conversationCalls += 1
            return conversationCalls
        }
        if n == holdCall {
            await withCheckedContinuation { c in lock.withLock { held = c } }
        }
        let answer = lock.withLock { conversationAnswers[min(n, conversationAnswers.count) - 1] }
        return try answer.get()
    }

    func messages(conversationId: String, before: MessageKey?, limit: UInt32) async throws -> [Message] { [] }
    func reactions(key: MessageKey) async throws -> [Reaction] { [] }
    func search(query: String, limit: UInt32) async throws -> [Message] { [] }
}

func conversation(_ id: String, unread: UInt32 = 0) -> Conversation {
    Conversation(id: id, kind: .direct, title: id, lastMessageAt: 1, lastMessagePreview: "", unreadCount: unread)
}

final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func increment() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

struct TestError: Error, CustomStringConvertible {
    let description: String
}

/// Polls the main actor until `condition` holds (or fails the test after ~2 s).
@MainActor
func eventually(
    _ condition: @MainActor () -> Bool, file: StaticString = #filePath, line: UInt = #line
) async {
    for _ in 0..<200 {
        if condition() { return }
        try? await Task.sleep(for: .milliseconds(10))
    }
    XCTFail("condition not met in time", file: file, line: line)
}
