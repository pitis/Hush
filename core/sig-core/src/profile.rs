//! Our own profile: the name and about Settings shows and edits. The picture stays the phone's
//! (presage can't upload one).

use presage::libsignal_service::prelude::ProfileKey;
use presage::libsignal_service::profile_cipher::ProfileCipher;
use presage::libsignal_service::profile_name::ProfileName;
use presage::libsignal_service::protocol::Aci;
use presage::libsignal_service::websocket::profile::SignalServiceProfile;
use presage::libsignal_service::Profile;

/// Our name and about as the server has them; empty for what isn't set.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct OwnProfile {
    pub given_name: String,
    pub family_name: String,
    pub about: String,
}

/// The profile's ciphertext fit Signal's paddings: a name (given, a 0, family) of at most 257
/// bytes, an about of at most 512.
const NAME_MAX: usize = 257;
const ABOUT_MAX: usize = 512;

/// What a save uploads: the name and about, trimmed. A first name is needed (Signal's apps ask
/// for one); a 0 would split the name where it isn't split.
pub fn edit(given: &str, family: &str, about: &str) -> Result<(ProfileName<String>, Option<String>), String> {
    let (given, family, about) = (given.trim(), family.trim(), about.trim());
    if given.is_empty() {
        return Err("a first name is needed".into());
    }
    if given.contains('\0') || family.contains('\0') {
        return Err("a name can't hold that character".into());
    }
    let name = ProfileName {
        given_name: given.to_string(),
        family_name: Some(family.to_string()).filter(|f| !f.is_empty()),
    };
    if name.serialize().len() > NAME_MAX {
        return Err("that name is too long".into());
    }
    if about.len() > ABOUT_MAX {
        return Err("that about is too long".into());
    }
    Ok((name, Some(about.to_string()).filter(|a| !a.is_empty())))
}

/// The about's emoji kept through a save: it goes with an about, so not when the about is cleared.
pub fn kept_emoji(about: &Option<String>, emoji: Option<String>) -> Option<String> {
    about.as_ref().and(emoji)
}

pub fn own(p: &Profile) -> OwnProfile {
    OwnProfile {
        given_name: p.name.as_ref().map(|n| n.given_name.clone()).unwrap_or_default(),
        family_name: p.name.as_ref().and_then(|n| n.family_name.clone()).unwrap_or_default(),
        about: p.about.clone().unwrap_or_default(),
    }
}

/// `GET /v1/profile/<aci>/<version>`: our profile at our key's version.
pub fn path(aci: Aci, key: ProfileKey) -> String {
    let version = key.get_profile_key_version(aci);
    format!("/v1/profile/{}/{}", aci.service_id_string(), version.as_ref())
}

/// The server's answer to `path`, decrypted with our key.
pub fn decrypt(body: &[u8], key: ProfileKey) -> Result<Profile, String> {
    let encrypted: SignalServiceProfile =
        serde_json::from_slice(body).map_err(|e| format!("the profile didn't read: {e}"))?;
    ProfileCipher::new(key)
        .decrypt(encrypted)
        .map_err(|e| format!("the profile didn't decrypt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn a_save_needs_a_first_name_and_trims() {
        assert_eq!(edit("  ", "Lovelace", "").unwrap_err(), "a first name is needed");
        let (name, about) = edit(" Ada ", " ", "  ").unwrap();
        assert_eq!(
            name,
            ProfileName {
                given_name: "Ada".into(),
                family_name: None
            }
        );
        assert_eq!(about, None);
        let (name, about) = edit("Ada", "Lovelace", " counting ").unwrap();
        assert_eq!(name.family_name.as_deref(), Some("Lovelace"));
        assert_eq!(about.as_deref(), Some("counting"));
    }

    #[test]
    fn a_save_fits_signals_paddings() {
        assert!(edit("A\0B", "", "").is_err());
        assert!(edit("A", "B\0", "").is_err());
        // given, the 0, family: 257 bytes fit, 258 don't
        assert!(edit(&"a".repeat(128), &"b".repeat(128), "").is_ok());
        assert_eq!(
            edit(&"a".repeat(128), &"b".repeat(129), "").unwrap_err(),
            "that name is too long"
        );
        assert!(edit("Ada", "", &"é".repeat(256)).is_ok());
        assert_eq!(edit("Ada", "", &"é".repeat(257)).unwrap_err(), "that about is too long");
    }

    #[test]
    fn the_emoji_goes_with_the_about() {
        assert_eq!(kept_emoji(&Some("hi".into()), Some("🙂".into())).as_deref(), Some("🙂"));
        assert_eq!(kept_emoji(&None, Some("🙂".into())), None);
        assert_eq!(kept_emoji(&Some("hi".into()), None), None);
    }

    #[test]
    fn the_servers_answer_decrypts_with_our_key() {
        let key = ProfileKey::create([7; 32]);
        let cipher = ProfileCipher::new(key);
        let rng = &mut rand::rng();
        let name = ProfileName {
            given_name: "Ada",
            family_name: Some("Lovelace"),
        };
        let b64 = |b: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(b);
        let body = serde_json::json!({
            "name": b64(cipher.encrypt_name(name, rng).unwrap()),
            "about": b64(cipher.encrypt_about("counting".into(), rng).unwrap()),
            "aboutEmoji": b64(cipher.encrypt_emoji("🙂".into(), rng).unwrap()),
            "avatar": "profiles/abc",
            "unrestrictedUnidentifiedAccess": false,
            "capabilities": {},
        });
        let p = decrypt(body.to_string().as_bytes(), key).unwrap();
        assert_eq!(
            own(&p),
            OwnProfile {
                given_name: "Ada".into(),
                family_name: "Lovelace".into(),
                about: "counting".into(),
            }
        );
        assert_eq!(p.about_emoji.as_deref(), Some("🙂"));
        assert!(decrypt(body.to_string().as_bytes(), ProfileKey::create([8; 32])).is_err());
        assert!(decrypt(b"not json", key).is_err());
    }

    #[test]
    fn nothing_set_reads_as_empty() {
        let body = br#"{"unrestrictedUnidentifiedAccess":false,"capabilities":{}}"#;
        let p = decrypt(body, ProfileKey::create([7; 32])).unwrap();
        assert_eq!(
            own(&p),
            OwnProfile {
                given_name: String::new(),
                family_name: String::new(),
                about: String::new(),
            }
        );
    }

    #[test]
    fn the_path_names_our_key_version() {
        let aci = Aci::from_uuid_bytes([1; 16]);
        let key = ProfileKey::create([7; 32]);
        let path = path(aci, key);
        let version = key.get_profile_key_version(aci);
        assert_eq!(
            path,
            format!("/v1/profile/01010101-0101-0101-0101-010101010101/{}", version.as_ref())
        );
    }
}
