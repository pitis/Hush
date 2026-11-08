//! The database key, created once per computer and kept in the OS keychain (macOS Keychain,
//! Windows Credential Manager, Secret Service on Linux). Like the Mac app's `Keychain.swift`, but
//! under its own name: the two apps never share a key or a data folder.

const SERVICE: &str = "dev.pitis.hush.desktop";
const ACCOUNT: &str = "db-key";

/// 32 random bytes as 64 lowercase hex characters; created on first use.
pub fn db_key() -> Result<String, String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| match keyring::Entry::store_status() {
        // the store's own reason; `e` only says there is no store
        Err(why) => {
            format!("the keychain isn't available ({why}); on Linux, Hush needs GNOME Keyring or KWallet running")
        }
        Ok(()) => format!("the keychain isn't available: {e}"),
    })?;
    key_in(&entry)
}

fn key_in(entry: &keyring::Entry) -> Result<String, String> {
    match entry.get_password() {
        Ok(hex) => Ok(hex),
        Err(keyring::Error::NoEntry) => {
            let hex = hex::encode(rand::random::<[u8; 32]>());
            entry
                .set_password(&hex)
                .map_err(|e| format!("couldn't keep the database key in the keychain: {e}"))?;
            // read back: of two first launches at once, both must use the key that stayed
            entry
                .get_password()
                .map_err(|e| format!("couldn't read the database key back from the keychain: {e}"))
        }
        // Any other failure must not fall through to creating a new key: that would make the
        // existing (encrypted) data unreadable.
        Err(e) => Err(format!("couldn't read the database key from the keychain: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use keyring_core::api::CredentialStoreApi as _;
    use keyring_core::mock;

    use super::*;

    fn entry() -> keyring::Entry {
        let inner = mock::Store::new().unwrap().build(SERVICE, ACCOUNT, None).unwrap();
        keyring::Entry { inner }
    }

    #[test]
    fn a_key_is_created_once_and_then_kept() {
        let entry = entry();
        let key = key_in(&entry).unwrap();
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(key_in(&entry).unwrap(), key);
    }

    #[test]
    fn an_unreadable_key_is_never_replaced() {
        let entry = entry();
        entry.set_secret(&[0xff, 0xfe]).unwrap();
        assert!(key_in(&entry).is_err());
        assert_eq!(entry.get_secret().unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn a_keychain_that_refuses_access_keeps_the_key() {
        let entry = entry();
        entry.set_password("old").unwrap();
        let cred = entry.inner.as_any().downcast_ref::<mock::Cred>().unwrap();
        cred.set_error(keyring::Error::NoStorageAccess("locked".into()));
        assert!(key_in(&entry).is_err());
        assert_eq!(entry.get_password().unwrap(), "old");
    }
}
