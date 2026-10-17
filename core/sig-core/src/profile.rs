//! Our own profile: the name and about Settings shows and edits. The picture stays the phone's
//! (presage can't upload one). Written here, not through presage's `update_profile`: its write
//! leaves out the payment address and the phone number sharing setting, which the server then
//! clears.

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

/// What `edit` let through, as Settings shows it after the save.
pub fn saved(name: &ProfileName<String>, about: &Option<String>) -> OwnProfile {
    OwnProfile {
        given_name: name.given_name.clone(),
        family_name: name.family_name.clone().unwrap_or_default(),
        about: about.clone().unwrap_or_default(),
    }
}

/// The server's profile holds what a save sent: it took, whatever the save answered.
pub fn took(p: &Profile, name: &ProfileName<String>, about: &Option<String>) -> bool {
    own(p) == saved(name, about)
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

/// What our profile holds that we don't read, as the server sent it (encrypted, base64): a
/// write sends it back, still valid under the same key and version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    pub payment_address: Option<String>,
    pub phone_number_sharing: Option<String>,
}

pub fn kept(body: &[u8]) -> Result<Kept, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("the profile didn't read: {e}"))?;
    let field = |name: &str| v[name].as_str().filter(|s| !s.is_empty()).map(String::from);
    Ok(Kept {
        payment_address: field("paymentAddress"),
        phone_number_sharing: field("phoneNumberSharing"),
    })
}

/// The body of `PUT /v1/profile`, as libsignal-service writes it (an empty about and emoji
/// encrypted too; the picture unchanged) and with what we keep.
pub fn write(
    aci: Aci,
    key: ProfileKey,
    name: &ProfileName<String>,
    about: &Option<String>,
    emoji: &Option<String>,
    kept: &Kept,
) -> Result<serde_json::Value, String> {
    use base64::Engine as _;
    let b64 = |b: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(b);
    let (cipher, rng) = (ProfileCipher::new(key), &mut rand::rng());
    let failed =
        |e: presage::libsignal_service::profile_cipher::ProfileCipherError| format!("the profile didn't encrypt: {e}");
    let mut body = serde_json::json!({
        "version": key.get_profile_key_version(aci).as_ref(),
        "name": b64(cipher.encrypt_name(name.as_ref(), rng).map_err(failed)?),
        "about": b64(cipher.encrypt_about(about.clone().unwrap_or_default(), rng).map_err(failed)?),
        "aboutEmoji": b64(cipher.encrypt_emoji(emoji.clone().unwrap_or_default(), rng).map_err(failed)?),
        "avatar": true,
        "sameAvatar": true,
        "commitment": b64(zkgroup::serialize(&key.get_commitment(aci))),
    });
    if let Some(a) = &kept.payment_address {
        body["paymentAddress"] = a.clone().into();
    }
    if let Some(s) = &kept.phone_number_sharing {
        body["phoneNumberSharing"] = s.clone().into();
    }
    Ok(body)
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
        let (name, about) = edit(" Ada ", " ", " hi ").unwrap();
        assert_eq!(
            saved(&name, &about),
            OwnProfile {
                given_name: "Ada".into(),
                family_name: String::new(),
                about: "hi".into(),
            }
        );
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
        // the limits are the cipher's: the longest accepted encrypt
        let (cipher, rng) = (ProfileCipher::new(ProfileKey::create([7; 32])), &mut rand::rng());
        let (name, about) = edit(&"a".repeat(128), &"b".repeat(128), &"é".repeat(256)).unwrap();
        assert!(cipher.encrypt_name(name.as_ref(), rng).is_ok());
        assert!(cipher.encrypt_about(about.unwrap(), rng).is_ok());
        assert_eq!(edit("Ada", "", &"é".repeat(257)).unwrap_err(), "that about is too long");
    }

    #[test]
    fn a_save_took_when_the_server_has_what_it_sent() {
        let (name, about) = edit("Ada", "", "counting").unwrap();
        let profile = |given: &str, about: Option<&str>| Profile {
            name: Some(ProfileName {
                given_name: given.into(),
                family_name: None,
            }),
            about: about.map(Into::into),
            about_emoji: None,
            avatar: None,
            unrestricted_unidentified_access: false,
        };
        assert!(took(&profile("Ada", Some("counting")), &name, &about));
        assert!(!took(&profile("Ada", None), &name, &about));
        assert!(!took(&profile("Bea", Some("counting")), &name, &about));
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
    fn a_write_sends_back_what_we_keep_and_reads_as_saved() {
        let aci = Aci::from_uuid_bytes([1; 16]);
        let key = ProfileKey::create([7; 32]);
        let answer = br#"{"paymentAddress":"cGF5","phoneNumberSharing":"c2hhcmU=","capabilities":{}}"#;
        let kept = kept(answer).unwrap();
        assert_eq!(kept.payment_address.as_deref(), Some("cGF5"));
        assert_eq!(kept.phone_number_sharing.as_deref(), Some("c2hhcmU="));
        let (name, about) = edit("Ada", "Lovelace", "counting").unwrap();
        let body = write(aci, key, &name, &about, &Some("🙂".into()), &kept).unwrap();
        assert_eq!(body["paymentAddress"], "cGF5");
        assert_eq!(body["phoneNumberSharing"], "c2hhcmU=");
        assert_eq!(
            (body["avatar"].as_bool(), body["sameAvatar"].as_bool()),
            (Some(true), Some(true))
        );
        assert_eq!(body["version"], key.get_profile_key_version(aci).as_ref());
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::STANDARD
                .decode(v.as_str().unwrap())
                .unwrap()
        };
        assert_eq!(b64(&body["commitment"]), zkgroup::serialize(&key.get_commitment(aci)));
        // the server's sizes: a name of 81 bytes, an about of 156, an emoji of 60
        assert_eq!(
            (
                b64(&body["name"]).len(),
                b64(&body["about"]).len(),
                b64(&body["aboutEmoji"]).len()
            ),
            (81, 156, 60)
        );
        // what the server would answer for it reads back as saved
        let p = decrypt(
            serde_json::json!({
                "name": body["name"], "about": body["about"], "aboutEmoji": body["aboutEmoji"], "capabilities": {},
            })
            .to_string()
            .as_bytes(),
            key,
        )
        .unwrap();
        assert!(took(&p, &name, &about));
        assert_eq!(p.about_emoji.as_deref(), Some("🙂"));
        // nothing kept: nothing sent; a cleared about is an empty one, as Signal's apps send it
        let body = write(aci, key, &name, &None, &None, &Kept::default()).unwrap();
        assert!(body.get("paymentAddress").is_none() && body.get("phoneNumberSharing").is_none());
        assert_eq!(b64(&body["about"]).len(), 156);
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
        // as libsignal-service spells it: the version's serialized bytes
        let version = String::from_utf8(zkgroup::serialize(&key.get_profile_key_version(aci))).unwrap();
        assert_eq!(version.len(), 64);
        assert_eq!(
            path,
            format!("/v1/profile/01010101-0101-0101-0101-010101010101/{version}")
        );
    }
}
