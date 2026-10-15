use presage::libsignal_service::prelude::Uuid;
use presage::store::Thread;

/// `c:<aci uuid>` for a 1:1 thread, `g:<hex master key>` for a group.
pub fn conversation_id(thread: &Thread) -> String {
    match thread {
        Thread::Contact(sid) => format!("c:{}", sid.raw_uuid()),
        Thread::Group(key) => format!("g:{}", hex::encode(key)),
    }
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
}
