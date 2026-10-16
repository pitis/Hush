use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use presage::store::Thread;

/// `c:<uuid>` for a 1:1 thread with an ACI, `c:PNI:<uuid>` with a phone-number identity (kept
/// distinct so it is never mistaken for an ACI), `g:<hex master key>` for a group.
pub fn conversation_id(thread: &Thread) -> String {
    match thread {
        Thread::Contact(sid) => direct_id(sid),
        Thread::Group(key) => group_id(key),
    }
}

pub fn direct_id(sid: &ServiceId) -> String {
    format!("c:{}", sid.service_id_string())
}

pub fn group_id(master_key: &[u8; 32]) -> String {
    format!("g:{}", hex::encode(master_key))
}

/// The group identifier of a `g:<hex master key>` chat: how the server, typing messages and
/// block lists name a group. None for anything else.
pub fn group_identifier(conversation_id: &str) -> Option<Vec<u8>> {
    let key: [u8; 32] = hex::decode(conversation_id.strip_prefix("g:")?).ok()?.try_into().ok()?;
    Some(
        GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key))
            .get_group_identifier()
            .to_vec(),
    )
}

/// An ACI as our canonical lowercase hyphenated UUID string, from either the string or the
/// 16-byte binary form Signal messages carry (newer clients may send only the binary one).
pub fn aci(string: Option<&str>, binary: Option<&[u8]>) -> Option<String> {
    binary
        .and_then(|b| Uuid::from_slice(b).ok())
        .or_else(|| string.and_then(|s| Uuid::parse_str(s).ok()))
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::protocol::{Aci, Pni};

    const U: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn aci_accepts_string_or_binary_and_normalizes() {
        let u = Uuid::parse_str(U).unwrap();
        assert_eq!(aci(Some(U), None).as_deref(), Some(U));
        assert_eq!(aci(Some(&U.to_uppercase()), None).as_deref(), Some(U));
        assert_eq!(aci(None, Some(u.as_bytes())).as_deref(), Some(U));
        assert_eq!(aci(Some("garbage"), Some(u.as_bytes())).as_deref(), Some(U));
        assert_eq!(aci(Some("garbage"), None), None);
        assert_eq!(aci(None, Some(&[1, 2, 3])), None);
        assert_eq!(aci(None, None), None);
    }

    #[test]
    fn direct_ids_keep_aci_and_pni_apart() {
        let u = Uuid::parse_str(U).unwrap();
        assert_eq!(direct_id(&ServiceId::from(Aci::from(u))), format!("c:{U}"));
        assert_eq!(direct_id(&ServiceId::from(Pni::from(u))), format!("c:PNI:{U}"));
        assert_eq!(group_id(&[0xab; 32]), format!("g:{}", "ab".repeat(32)));
    }

    #[test]
    fn group_identifiers_come_from_the_master_key() {
        let a = group_identifier(&group_id(&[0xab; 32])).unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, vec![0xab; 32]);
        assert_ne!(Some(a), group_identifier(&group_id(&[0xcd; 32])));
        assert_eq!(group_identifier("c:abab"), None);
        assert_eq!(group_identifier("g:zz"), None);
    }
}
