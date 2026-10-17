import Foundation

/// The app's old name. Data, the database key and settings kept under it are moved to Hush's own
/// once (`AppStore.dataDirectory`, `Keychain.dbKey`, `adoptSettings`).
enum Legacy {
    static let name = "Semaphore"
    static let bundleId = "dev.pitis.semaphore"

    /// Copies the settings saved under the old bundle id, keeping any already set, then removes them.
    static func adoptSettings(into defaults: UserDefaults = .standard, from domain: String = bundleId) {
        guard let old = defaults.persistentDomain(forName: domain) else { return }
        for (key, value) in old where defaults.object(forKey: key) == nil {
            defaults.set(value, forKey: key)
        }
        defaults.removePersistentDomain(forName: domain)
    }
}
