//! Safety numbers: telling when a contact's identity key changes, and the number to compare.

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{
    DeviceId, Fingerprint, IdentityKey, IdentityKeyStore, ProtocolAddress, ServiceId,
};
use presage::store::{ContentsStore as _, Store as _};
use presage_store_sqlite::SqliteStore;

use crate::events::{CoreEvent, EventListener};
use crate::projection::{ChatEventKind, ProjectionDb};
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

/// Checks each of `acis` (but us, `me`) in `store` (presage's keys as our ACI or our PNI got
/// them) against the identity key noted last (`ProjectionDb::note_identity_key`), and tells the
/// app of the chats a change was shown in. presage trusts a new key silently, so this runs after
/// what can bring one (a message from them, a send to them); a check that fails or doesn't run is
/// caught up by the next one, as the noted key stays the old one.
pub(crate) async fn notice(
    store: &impl IdentityKeyStore,
    db: &ProjectionDb,
    l: &dyn EventListener,
    me: &str,
    acis: Vec<String>,
    at: i64,
) {
    let acis: Vec<String> = acis.into_iter().filter(|aci| aci != me).collect();
    // logged, not shown: this runs on every message, and the next check tries again
    let noted = match db.noted_identity_keys(&acis).await {
        Ok(noted) => noted,
        Err(e) => return tracing::warn!(error = %e, "reading the noted identity keys failed"),
    };
    for aci in acis {
        let key = match store.get_identity(&address(&aci)).await {
            Ok(Some(key)) => key.serialize(),
            Ok(None) => continue, // no session with them yet
            Err(e) => {
                tracing::warn!(error = %e, "reading an identity key failed");
                continue;
            }
        };
        if noted.get(&aci).is_some_and(|noted| **noted == *key) {
            continue;
        }
        match db.note_identity_key(&aci, &key, me, at).await {
            Ok(chats) => {
                if !chats.is_empty() {
                    let contact = db.opaque_id(&format!("c:{aci}"));
                    tracing::info!(contact, chats = chats.len(), "a safety number changed");
                }
                for conversation_id in chats {
                    l.on_event(CoreEvent::ChatEventAdded {
                        conversation_id,
                        kind: ChatEventKind::SafetyNumberChanged,
                    });
                }
            }
            Err(e) => tracing::warn!(error = %e, "noting an identity key failed"),
        }
    }
}

/// `notice` for the people of a chat we just sent to: a send can bring new keys (a new session,
/// or the server's 409/410 naming devices we didn't know).
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
    notice(&store.aci_protocol_store(), db, l, me, acis, at).await;
}

/// `notice` for everyone Hush knows (1:1 chats, group members, presage's contacts), before
/// receiving starts: a key never noted (the first run of this version) is noted before their next
/// message can bring a new one, and a change a failed check missed is told.
pub(crate) async fn notice_known(store: &SqliteStore, db: &ProjectionDb, l: &dyn EventListener, me: &str) {
    let mut acis = match db.known_acis().await {
        Ok(acis) => acis,
        Err(e) => return tracing::warn!(error = %e, "reading who to check failed"),
    };
    match store.contacts().await {
        Ok(contacts) => acis.extend(contacts.flatten().map(|c| c.uuid.to_string())),
        Err(e) => tracing::warn!(error = %e, "reading the contacts to check failed"),
    }
    acis.sort();
    acis.dedup();
    let now = chrono::Utc::now().timestamp_millis();
    notice(&store.aci_protocol_store(), db, l, me, acis, now).await;
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

    struct Sink(std::sync::Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    /// A presage store in a folder of its own, and an empty projection.
    struct Stores {
        dir: std::path::PathBuf,
        presage: SqliteStore,
        db: ProjectionDb,
        sink: Sink,
    }

    impl Stores {
        async fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sig-core-safety-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let presage = SqliteStore::open_with_options(
                crate::actor::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
                presage::model::identity::OnNewIdentity::Trust,
            )
            .await
            .unwrap();
            let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
            Self {
                dir,
                presage,
                db,
                sink: Sink(Default::default()),
            }
        }

        /// The chats told of a change so far.
        fn told(&self) -> Vec<String> {
            let events = self.sink.0.lock().unwrap();
            events
                .iter()
                .map(|e| match e {
                    CoreEvent::ChatEventAdded {
                        conversation_id,
                        kind: ChatEventKind::SafetyNumberChanged,
                    } => conversation_id.clone(),
                    other => panic!("{other:?}"),
                })
                .collect()
        }

        async fn check(&self, acis: &[&String], at: i64) {
            let acis = acis.iter().map(|a| a.to_string()).collect();
            notice(&self.presage.aci_protocol_store(), &self.db, &self.sink, ME, acis, at).await;
        }
    }

    impl Drop for Stores {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const ME: &str = "01010101-0101-0101-0101-010101010101";

    fn aci(n: u8) -> String {
        Uuid::from_bytes([n; 16]).to_string()
    }

    fn new_key() -> IdentityKey {
        *presage::libsignal_service::protocol::IdentityKeyPair::generate(&mut rand::rng()).identity_key()
    }

    /// What presage does with a key a message or a send brings: kept, whatever it was.
    async fn save(store: &mut impl IdentityKeyStore, aci: &str, device: u8, key: &IdentityKey) {
        let address = ProtocolAddress::new(aci.to_string(), DeviceId::new(device).unwrap());
        store.save_identity(&address, key).await.unwrap();
    }

    /// presage's store keeps the new key silently; the check sees it once, not on first contact,
    /// and never for us.
    #[tokio::test]
    async fn a_new_key_in_presages_store_is_told_once() {
        let s = Stores::new("once").await;
        let (alice, me) = (aci(2), ME.to_string());
        let mut keys = s.presage.aci_protocol_store();
        s.check(&[&alice], 10).await;
        assert!(s.told().is_empty(), "no key yet: no session");
        save(&mut keys, &alice, 1, &new_key()).await;
        save(&mut keys, &me, 1, &new_key()).await;
        s.check(&[&alice, &me], 20).await;
        assert!(s.told().is_empty(), "first contact");
        save(&mut keys, &alice, 1, &new_key()).await;
        save(&mut keys, &me, 1, &new_key()).await;
        s.check(&[&alice, &me], 30).await;
        s.check(&[&alice], 40).await;
        assert_eq!(s.told(), [format!("c:{alice}")]);
        let events = s.db.chat_events(&format!("c:{alice}"), 0).await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.kind, e.at, e.author.as_str()))
                .collect::<Vec<_>>(),
            [(ChatEventKind::SafetyNumberChanged, 30, alice.as_str())]
        );
        assert!(s.db.chat_events(&format!("c:{me}"), 0).await.unwrap().is_empty());
        assert!(
            s.db.noted_identity_keys(&[me]).await.unwrap().is_empty(),
            "ours is never noted"
        );
    }

    /// presage keeps one key per contact, whichever device brought it.
    #[tokio::test]
    async fn a_key_from_another_device_is_the_same_contacts() {
        let s = Stores::new("devices").await;
        let alice = aci(2);
        let mut keys = s.presage.aci_protocol_store();
        save(&mut keys, &alice, 1, &new_key()).await;
        s.check(&[&alice], 10).await;
        let new = new_key();
        save(&mut keys, &alice, 2, &new).await;
        s.check(&[&alice], 20).await;
        save(&mut keys, &alice, 2, &new).await;
        save(&mut keys, &alice, 1, &new).await;
        s.check(&[&alice], 30).await;
        assert_eq!(s.told(), [format!("c:{alice}")]);
    }

    /// presage decrypts what was sent to our PNI with our PNI's store, and keeps the key it
    /// brought there.
    #[tokio::test]
    async fn a_new_key_brought_to_our_pni_is_told() {
        let s = Stores::new("pni").await;
        let alice = aci(2);
        save(&mut s.presage.aci_protocol_store(), &alice, 1, &new_key()).await;
        s.check(&[&alice], 10).await;
        save(&mut s.presage.pni_protocol_store(), &alice, 1, &new_key()).await;
        notice(
            &s.presage.pni_protocol_store(),
            &s.db,
            &s.sink,
            ME,
            vec![alice.clone()],
            20,
        )
        .await;
        assert_eq!(s.told(), [format!("c:{alice}")]);
    }

    /// After a send: the chat's people are checked, a group's all at once.
    #[tokio::test]
    async fn a_send_checks_the_people_it_went_to() {
        let s = Stores::new("send").await;
        let (alice, bob) = (aci(2), aci(3));
        let group = format!("g:{}", "ab".repeat(32));
        s.db.set_group(
            &group,
            1,
            "Hikers",
            &[ME.into(), alice.clone(), bob.clone()],
            0,
            crate::projection::GroupStanding::Other,
        )
        .await
        .unwrap();
        let mut keys = s.presage.aci_protocol_store();
        for who in [&alice, &bob, &ME.to_string()] {
            save(&mut keys, who, 1, &new_key()).await;
        }
        notice_chat(&s.presage, &s.db, &s.sink, ME, &group, 10).await;
        save(&mut keys, &alice, 1, &new_key()).await;
        save(&mut keys, ME, 1, &new_key()).await;
        notice_chat(&s.presage, &s.db, &s.sink, ME, &group, 20).await;
        assert_eq!(s.told(), [format!("c:{alice}"), group.clone()]);
        // a phone number's chat has no ACI to check
        notice_chat(&s.presage, &s.db, &s.sink, ME, &format!("c:PNI:{bob}"), 30).await;
        save(&mut keys, &bob, 1, &new_key()).await;
        notice_chat(&s.presage, &s.db, &s.sink, ME, &format!("c:{bob}"), 40).await;
        assert_eq!(
            s.told(),
            [format!("c:{alice}"), group.clone(), format!("c:{bob}"), group]
        );
    }

    /// Everyone known gets their key noted before receiving starts, without a word: a change
    /// before their next message is then told.
    #[tokio::test]
    async fn keys_never_noted_are_noted_before_receiving() {
        let s = Stores::new("known").await;
        let (alice, bob, carol) = (aci(2), aci(3), aci(4));
        s.db.upsert_conversation_title(&format!("c:{alice}"), "Alice")
            .await
            .unwrap();
        s.db.set_group(
            &format!("g:{}", "ab".repeat(32)),
            1,
            "Hikers",
            &[ME.into(), bob.clone()],
            0,
            crate::projection::GroupStanding::Other,
        )
        .await
        .unwrap();
        let contact = presage::model::contacts::Contact {
            uuid: Uuid::parse_str(&carol).unwrap(),
            phone_number: None,
            name: String::new(),
            verified: Default::default(),
            profile_key: Vec::new(),
            expire_timer: 0,
            expire_timer_version: 2,
            inbox_position: 0,
            avatar: None,
        };
        s.presage.clone().save_contact(&contact).await.unwrap();
        let mut keys = s.presage.aci_protocol_store();
        for who in [&alice, &bob, &carol] {
            save(&mut keys, who, 1, &new_key()).await;
        }
        notice_known(&s.presage, &s.db, &s.sink, ME).await;
        assert!(s.told().is_empty());
        for who in [&alice, &bob, &carol] {
            save(&mut keys, who, 1, &new_key()).await;
            s.check(&[who], 10).await;
        }
        assert_eq!(s.told().len(), 4, "alice, bob (and the group), carol");
    }

    /// The number shown is libsignal-service's for the same two keys.
    #[tokio::test]
    async fn our_safety_number_is_the_one_libsignal_service_computes() {
        use presage::libsignal_service::protocol::Aci;
        use presage::libsignal_service::session_store::SessionStoreExt;
        use presage::store::StateStore;
        let s = Stores::new("ours").await;
        let (me, them) = (Uuid::from_bytes([1; 16]), Uuid::from_bytes([2; 16]));
        let pair = presage::libsignal_service::protocol::IdentityKeyPair::generate(&mut rand::rng());
        s.presage.set_aci_identity_key_pair(pair).await.unwrap();
        assert_eq!(
            ours_with(&s.presage, me, them).await.unwrap(),
            None,
            "no key of theirs yet"
        );
        save(&mut s.presage.aci_protocol_store(), &them.to_string(), 1, &new_key()).await;
        let ours = ours_with(&s.presage, me, them).await.unwrap().unwrap();
        let expected = s
            .presage
            .aci_protocol_store()
            .compute_safety_number(&Aci::from(me).into(), &Aci::from(them).into())
            .await
            .unwrap();
        assert_eq!(ours.digits, expected);
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
