import Foundation

/// The app's old name. Data, the database key and settings kept under it are moved to Hush's own
/// once (`AppStore.dataDirectory`, `Keychain.dbKey`, `adoptSettings`).
enum Legacy {
    static let name = "Semaphore"
    static let bundleId = "dev.pitis.semaphore"

    enum MoveError: Error {
        /// The old app (or its core) still has the data folder open.
        case stillOpen
    }

    /// Runs `move` holding the data folder's lock (the one the core takes, a `flock` on `.lock`),
    /// so the folder never moves out from under a running copy of the old app.
    static func whileClosed(_ folder: URL, _ move: () throws -> Void) throws {
        let lock = open(folder.appendingPathComponent(".lock").path, O_RDONLY)
        guard lock >= 0 else { return try move() }  // never opened by a core
        defer { close(lock) }
        guard flock(lock, LOCK_EX | LOCK_NB) == 0 else { throw MoveError.stillOpen }
        try move()
    }

    /// Copies the settings saved under the old bundle id, keeping any already set, then removes them.
    static func adoptSettings(into defaults: UserDefaults = .standard, from domain: String = bundleId) {
        guard let old = defaults.persistentDomain(forName: domain) else { return }
        for (key, value) in old where defaults.object(forKey: key) == nil {
            defaults.set(value, forKey: key)
        }
        defaults.removePersistentDomain(forName: domain)
    }
}
