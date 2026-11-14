//! Safety numbers: telling when a contact's identity key changes, and the number to compare.

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{
    DeviceId, Fingerprint, IdentityKey, IdentityKeyStore, ProtocolAddress, ServiceId,
};
use presage::store::Store as _;
use presage_store_sqlite::SqliteStore;

use crate::events::{CoreEvent, EventListener};
use crate::projection::ProjectionDb;
use crate::send::Recipient;

/// Our safety number with a contact, as both of us see it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct SafetyNumber {
    /// 60 digits.
    pub digits: String,
    /// What its QR code holds: libsignal's scannable fingerprint, as Signal's apps scan it.
    pub scannable: Vec<u8>,
}

/// The safety number between `local` and `remote` (identifier and identity key each). Signal's
/// apps now identify both sides by ACI (16 raw bytes): version 2, 5200 iterations, as
/// libsignal-service's `compute_safety_number` does (it gives only the digits).
pub(crate) fn safety_number(
    local_id: &[u8],
    local_key: &IdentityKey,
    remote_id: &[u8],
    remote_key: &IdentityKey,
) -> Result<SafetyNumber, String> {
    let fingerprint =
        Fingerprint::new(2, 5200, local_id, local_key, remote_id, remote_key).map_err(|e| e.to_string())?;
    Ok(SafetyNumber {
        digits: fingerprint.display_string().map_err(|e| e.to_string())?,
        scannable: fingerprint.scannable.serialize().map_err(|e| e.to_string())?,
    })
}

/// Checks each of `acis` (but us, `me`) in presage's store against the identity key noted last
/// (`ProjectionDb::note_identity_key`) and tells the app of the chats a change was shown in.
/// presage trusts a new key silently, so this runs after what can bring one (a message from
/// them, a send to them); a check that fails or doesn't run is caught up by the next one, as the
/// noted key stays the old one.
pub(crate) async fn notice(
    store: &SqliteStore,
    db: &ProjectionDb,
    l: &dyn EventListener,
    me: &str,
    acis: impl IntoIterator<Item = String>,
    at: i64,
) {
    let store = store.aci_protocol_store();
    for aci in acis {
        if aci == me {
            continue;
        }
        // logged, not shown: this runs on every message, and the next check tries again
        let key = match store.get_identity(&address(&aci)).await {
            Ok(Some(key)) => key,
            Ok(None) => continue, // no session with them yet
            Err(e) => {
                tracing::warn!(aci, error = %e, "reading an identity key failed");
                continue;
            }
        };
        match db.note_identity_key(&aci, &key.serialize(), me, at).await {
            Ok(chats) => {
                if !chats.is_empty() {
                    tracing::info!(aci, chats = chats.len(), "a safety number changed");
                }
                for conversation_id in chats {
                    l.on_event(CoreEvent::ChatEventAdded { conversation_id });
                }
            }
            Err(e) => tracing::warn!(aci, error = %e, "noting an identity key failed"),
        }
    }
}

/// `notice` for the people of a chat we just sent to: a send can bring new keys (a new session,
/// or the server's 409/410 naming devices we didn't know).
// shortcut: a group's members are checked one by one, two queries each; batch them if large
// groups make sending slow
pub(crate) async fn notice_chat(
    store: &SqliteStore,
    db: &ProjectionDb,
    l: &dyn EventListener,
    me: &str,
    conversation_id: &str,
    at: i64,
) {
    let acis = match crate::send::recipient(conversation_id) {
        Some(Recipient::Direct(ServiceId::Aci(aci))) => vec![aci.service_id_string()],
        Some(Recipient::Group(_)) => match db.group_members(conversation_id).await {
            Ok(members) => members,
            Err(e) => return tracing::warn!(error = %e, "reading a group's members failed"),
        },
        _ => return,
    };
    notice(store, db, l, me, acis, at).await;
}

/// Our (`me`) safety number with `them` (ACIs); None before any session with them (no key of
/// theirs yet).
pub(crate) async fn ours_with(store: &SqliteStore, me: Uuid, them: Uuid) -> Result<Option<SafetyNumber>, String> {
    let store = store.aci_protocol_store();
    let ours = store.get_identity_key_pair().await.map_err(|e| e.to_string())?;
    let Some(theirs) = store
        .get_identity(&address(&them.to_string()))
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    safety_number(me.as_bytes(), ours.identity_key(), them.as_bytes(), &theirs).map(Some)
}

/// Where presage keeps an ACI's identity key: one per account, whatever the device.
fn address(aci: &str) -> ProtocolAddress {
    ProtocolAddress::new(aci.to_string(), DeviceId::new(1).expect("1 is a device id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // libsignal's own vectors (rust/protocol/src/fingerprint.rs, `fingerprint_test_v2`)
    const ALICE_IDENTITY: &str = "0506863bc66d02b40d27b8d49ca7c09e9239236f9d7d25d6fcca5ce13c7064d868";
    const BOB_IDENTITY: &str = "05f781b6fb32fed9ba1cf2de978d4d5da28dc34046ae814402b5c0dbd96fda907b";
    const DISPLAYABLE: &str = "300354477692869396892869876765458257569162576843440918079131";
    const ALICE_SCANNABLE_V2: &str = "080212220a201e301a0353dce3dbe7684cb8336e85136cdc0ee96219494ada305d62a7bd61df1a220a20d62cbf73a11592015b6b9f1682ac306fea3aaf3885b84d12bca631e9d4fb3a4d";

    /// presage's store keeps the new key silently; the check sees it once, not on first contact,
    /// and never for us.
    #[tokio::test]
    async fn a_new_key_in_presages_store_is_told_once() {
        use crate::projection::ChatEventKind;
        use presage::libsignal_service::protocol::IdentityKeyPair;
        use presage::model::identity::OnNewIdentity;
        use std::sync::Mutex;

        struct Sink(Mutex<Vec<CoreEvent>>);
        impl EventListener for Sink {
            fn on_event(&self, e: CoreEvent) {
                self.0.lock().unwrap().push(e);
            }
        }
        let dir = std::env::temp_dir().join(format!("sig-core-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteStore::open_with_options(
            crate::actor::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let sink = Sink(Mutex::new(Vec::new()));
        let (me, alice) = (
            Uuid::from_bytes([1; 16]).to_string(),
            Uuid::from_bytes([2; 16]).to_string(),
        );
        let save = |aci: String| {
            let mut s = store.aci_protocol_store();
            async move {
                let key = IdentityKeyPair::generate(&mut rand::rng()).identity_key().to_owned();
                s.save_identity(&address(&aci), &key).await.unwrap();
            }
        };
        let told = |sink: &Sink| sink.0.lock().unwrap().len();

        notice(&store, &db, &sink, &me, [alice.clone()], 10).await;
        assert_eq!(told(&sink), 0, "no key yet: no session");
        save(alice.clone()).await;
        notice(&store, &db, &sink, &me, [alice.clone()], 20).await;
        assert_eq!(told(&sink), 0, "first contact");
        save(alice.clone()).await;
        save(me.clone()).await;
        save(me.clone()).await;
        notice(&store, &db, &sink, &me, [alice.clone(), me.clone()], 30).await;
        notice(&store, &db, &sink, &me, [alice.clone()], 40).await;
        let chat = format!("c:{alice}");
        assert!(
            matches!(&sink.0.lock().unwrap()[..], [CoreEvent::ChatEventAdded { conversation_id }] if *conversation_id == chat)
        );
        let events = db.chat_events(&chat, 0).await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.kind, e.at, e.author.as_str()))
                .collect::<Vec<_>>(),
            [(ChatEventKind::SafetyNumberChanged, 30, alice.as_str())]
        );
        assert!(db.chat_events(&format!("c:{me}"), 0).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_safety_number_is_libsignals_version_2() {
        let alice = IdentityKey::decode(&hex::decode(ALICE_IDENTITY).unwrap()).unwrap();
        let bob = IdentityKey::decode(&hex::decode(BOB_IDENTITY).unwrap()).unwrap();
        let ours = safety_number(b"+14152222222", &alice, b"+14153333333", &bob).unwrap();
        assert_eq!(ours.digits, DISPLAYABLE);
        assert_eq!(hex::encode(&ours.scannable), ALICE_SCANNABLE_V2);
        let theirs = safety_number(b"+14153333333", &bob, b"+14152222222", &alice).unwrap();
        assert_eq!(theirs.digits, ours.digits, "both sides see the same digits");
    }
}
