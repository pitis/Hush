//! The database key, created once per computer and kept in the OS keychain (macOS Keychain,
//! Windows Credential Manager, Secret Service on Linux). Like the Mac app's `Keychain.swift`, but
//! under its own name: the two apps never share a key or a data folder.

const SERVICE: &str = "dev.pitis.hush.desktop";
const ACCOUNT: &str = "db-key";

/// 32 random bytes as 64 lowercase hex characters; created on first use.
pub fn db_key() -> Result<String, String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| format!("the keychain isn't available: {e}"))?;
    match entry.get_password() {
        Ok(hex) => Ok(hex),
        Err(keyring::Error::NoEntry) => {
            let hex = hex::encode(rand::random::<[u8; 32]>());
            entry
                .set_password(&hex)
                .map_err(|e| format!("couldn't keep the database key in the keychain: {e}"))?;
            Ok(hex)
        }
        // Any other failure must not fall through to creating a new key: that would make the
        // existing (encrypted) data unreadable.
        Err(e) => Err(format!("couldn't read the database key from the keychain: {e}")),
    }
}
