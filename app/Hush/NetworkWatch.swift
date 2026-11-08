import AppKit
import Network

/// Tells when the network comes back (Wi-Fi on again, a cable plugged in) or the Mac wakes from
/// sleep, so the core can reconnect at once instead of after its backoff.
final class NetworkWatch: @unchecked Sendable {  // `was` is only touched on the monitor's serial queue
    private let monitor = NWPathMonitor()
    private var was: Bool?
    private var wakeObserver: NSObjectProtocol?

    /// Reconnect only when the network returns, not on every path report.
    static func shouldReconnect(was: Bool?, now: Bool) -> Bool { was == false && now }

    /// A sleep can keep the path satisfied throughout, leaving the old connection dead until its
    /// keepalive fails: reconnect on waking, but only with a network (without one the attempt
    /// fails, and the path's return reconnects anyway).
    static func shouldReconnectOnWake(_ path: NWPath.Status) -> Bool { path == .satisfied }

    init(onReturn: @escaping @MainActor () -> Void) {
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            let now = path.status == .satisfied
            if Self.shouldReconnect(was: was, now: now) { Task { @MainActor in onReturn() } }
            was = now
        }
        monitor.start(queue: DispatchQueue(label: "dev.pitis.hush.network"))
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: .main
        ) { [monitor] _ in
            if Self.shouldReconnectOnWake(monitor.currentPath.status) { Task { @MainActor in onReturn() } }
        }
    }

    deinit {
        monitor.cancel()
        if let wakeObserver { NSWorkspace.shared.notificationCenter.removeObserver(wakeObserver) }
    }
}
