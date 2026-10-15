import Network

/// Tells when the network comes back (Wi-Fi on again, a cable plugged in), so the core can
/// reconnect at once instead of after its backoff.
final class NetworkWatch: @unchecked Sendable {  // `was` is only touched on the monitor's serial queue
    private let monitor = NWPathMonitor()
    private var was: Bool?

    /// Reconnect only when the network returns, not on every path report.
    static func shouldReconnect(was: Bool?, now: Bool) -> Bool { was == false && now }

    init(onReturn: @escaping @MainActor () -> Void) {
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            let now = path.status == .satisfied
            if Self.shouldReconnect(was: was, now: now) { Task { @MainActor in onReturn() } }
            was = now
        }
        monitor.start(queue: DispatchQueue(label: "dev.pitis.semaphore.network"))
    }

    deinit { monitor.cancel() }
}
