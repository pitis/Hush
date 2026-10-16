//! The sync itself: `pull` the phone's state into the projection, `push` our changes back, and
//! `run` = pull then push with conflict retries. Everything here is tested against a fake
//! `StorageApi` and an in-memory projection.

use std::collections::HashMap;

use crate::projection::{ProjectionDb, StorageItemRow, StorageState};
use crate::storage::client::{StorageApi, StorageError};
use crate::storage::{records, Pin, Remote};

/// What the sync knows of the storage key: its SHA-256, so a new key is noticed.
pub(crate) struct Keys {
    pub fingerprint: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct PullOutcome {
    /// Records read and applied this time.
    pub applied: usize,
    /// Some chat's flags or the pins changed: the list wants announcing.
    pub chats_changed: bool,
    /// The manifest version the projection holds now.
    pub version: u64,
}

/// The phone's state into the projection: the manifest's records we haven't seen, applied with
/// the phone's values winning (spec: "Pull"). Nothing is written before every new item decoded.
pub(crate) async fn pull(
    api: &mut impl StorageApi,
    db: &ProjectionDb,
    keys: &Keys,
    me: &str,
) -> Result<PullOutcome, StorageError> {
    let mut state = db.storage_state().await?;
    // a pending pin change survives a reset (it is ours, not the old manifest's)
    let account_dirty = state.as_ref().is_some_and(|s| s.account_dirty);
    if state.as_ref().is_some_and(|s| s.key_fingerprint != keys.fingerprint) {
        tracing::info!("the storage key changed: the sync starts from nothing");
        db.reset_storage_state().await?;
        state = None;
    }
    let version = state.as_ref().map_or(0, |s| s.version);
    let Some(manifest) = api.manifest_if_changed(version).await? else {
        return Ok(PullOutcome {
            applied: 0,
            chats_changed: false,
            version,
        });
    };
    let stored: HashMap<Vec<u8>, StorageItemRow> = db
        .storage_items()
        .await?
        .into_iter()
        .map(|r| (r.key.clone(), r))
        .collect();
    let wanted: Vec<Vec<u8>> = manifest
        .identifiers
        .iter()
        .filter(|(k, _)| !stored.contains_key(k))
        .map(|(k, _)| k.clone())
        .collect();
    let gone = stored
        .keys()
        .filter(|k| !manifest.identifiers.iter().any(|(m, _)| m == *k))
        .count();
    let items = api.read_items(&wanted, &manifest.record_ikm).await?;
    let missing = wanted.len() - items.len();
    // decode everything before touching the projection: one bad item and the whole pull waits
    let mut decoded: HashMap<Vec<u8>, Remote> = HashMap::with_capacity(items.len());
    for (key, plain) in items {
        decoded.insert(key, records::decode(&plain).map_err(StorageError::Invalid)?);
    }
    // chats first, the account last: a remote pin wins over a remote archive of the same chat
    let mut changed = false;
    let mut account = None;
    let mut rows = Vec::with_capacity(manifest.identifiers.len());
    for (key, kind) in &manifest.identifiers {
        let conversation_id = match decoded.get(key) {
            Some(Remote::Chat { conversation_id, flags }) => {
                changed |= db.apply_remote_chat(conversation_id, flags).await?;
                Some(conversation_id.clone())
            }
            Some(Remote::Account { pins, note_to_self }) => {
                account = Some((pins.clone(), *note_to_self));
                None
            }
            Some(Remote::Other) => None,
            None => match stored.get(key) {
                Some(row) => row.conversation_id.clone(),
                // listed, but the server didn't return it: not ours to list
                None => continue,
            },
        };
        rows.push(StorageItemRow {
            key: key.clone(),
            kind: *kind,
            conversation_id,
        });
    }
    let mut unmappable_pins = state.map(|s| s.unmappable_pins).unwrap_or_default();
    if let Some((pins, note_to_self)) = account {
        let mut ids = vec![];
        let mut raw = vec![];
        for pin in pins {
            match pin {
                Pin::Chat(id) => ids.push(id),
                Pin::Raw(bytes) => raw.push(bytes),
            }
        }
        changed |= db.apply_remote_pins(&ids).await?;
        changed |= db
            .apply_remote_note_to_self(&format!("c:{me}"), note_to_self.0, note_to_self.1)
            .await?;
        unmappable_pins = raw;
    }
    db.replace_storage_items(&rows).await?;
    db.set_storage_state(&StorageState {
        version: manifest.version,
        record_ikm: manifest.record_ikm.clone(),
        key_fingerprint: keys.fingerprint.clone(),
        account_dirty,
        unmappable_pins,
    })
    .await?;
    tracing::info!(
        version = manifest.version,
        read = wanted.len(),
        applied = decoded.len(),
        gone,
        missing,
        "storage pull"
    );
    Ok(PullOutcome {
        applied: decoded.len(),
        chats_changed: changed,
        version: manifest.version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ChatFlags, ProjectionDb, StorageItemRow, StorageState};
    use crate::storage::client::{Manifest, StorageApi, StorageError, Write};
    use crate::storage::{KIND_ACCOUNT, KIND_CONTACT, KIND_GROUPV2};
    use presage::libsignal_service::prelude::ProtobufMessage as _;
    use presage::libsignal_service::proto::{
        account_record::{pinned_conversation, PinnedConversation},
        storage_record::Record,
        AccountRecord, ContactRecord, GroupV2Record, StorageRecord,
    };
    use std::collections::{HashMap, HashSet};

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const BOB: &str = "33333333-3333-3333-3333-333333333333";
    const KIND_CHAT_FOLDER: i32 = 8;

    /// The service as the tests script it: a manifest (None answers "unchanged"), the items it
    /// returns, the ones that won't decrypt, the writes it took, and how many it refuses first.
    #[derive(Default)]
    struct FakeApi {
        manifest: Option<Manifest>,
        items: HashMap<Vec<u8>, Vec<u8>>,
        fail_items: HashSet<Vec<u8>>,
        writes: Vec<Write>,
        conflicts_left: u32,
        unauthorized: bool,
        reads: Vec<Vec<Vec<u8>>>,
        manifest_calls: Vec<u64>,
    }

    impl StorageApi for FakeApi {
        async fn manifest_if_changed(&mut self, version: u64) -> Result<Option<Manifest>, StorageError> {
            self.manifest_calls.push(version);
            if self.unauthorized {
                return Err(StorageError::Unauthorized);
            }
            Ok(self.manifest.clone().filter(|m| m.version != version))
        }

        async fn read_items(&mut self, keys: &[Vec<u8>], _ikm: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StorageError> {
            self.reads.push(keys.to_vec());
            if keys.iter().any(|k| self.fail_items.contains(k)) {
                return Err(StorageError::Invalid("a storage item".into()));
            }
            Ok(keys
                .iter()
                .filter_map(|k| self.items.get(k).map(|v| (k.clone(), v.clone())))
                .collect())
        }

        async fn write(&mut self, w: &Write) -> Result<(), StorageError> {
            if self.unauthorized {
                return Err(StorageError::Unauthorized);
            }
            if self.conflicts_left > 0 {
                // someone else wrote first: a new version with one more contact in it
                self.conflicts_left -= 1;
                let m = self.manifest.get_or_insert_with(Manifest::default);
                m.version += 1;
                let k = vec![0xE0 + self.conflicts_left as u8; 16];
                m.identifiers.push((k.clone(), KIND_CONTACT));
                self.items.insert(k, contact(BOB, ChatFlags::default()));
                return Err(StorageError::Conflict);
            }
            self.writes.push(w.clone());
            self.manifest = Some(w.manifest.clone());
            for (k, v) in &w.inserts {
                self.items.insert(k.clone(), v.clone());
            }
            for k in &w.deletes {
                self.items.remove(k);
            }
            Ok(())
        }
    }

    fn key(n: u8) -> Vec<u8> {
        vec![n; 16]
    }

    fn keys() -> Keys {
        Keys {
            fingerprint: vec![1; 32],
        }
    }

    fn wrap(r: Record) -> Vec<u8> {
        StorageRecord { record: Some(r) }.encode_to_vec()
    }

    fn contact(aci: &str, f: ChatFlags) -> Vec<u8> {
        wrap(Record::Contact(ContactRecord {
            aci: aci.into(),
            archived: f.archived,
            marked_unread: f.marked_unread,
            muted_until_timestamp: f.mute_until as u64,
            blocked: f.blocked,
            whitelisted: f.whitelisted,
            ..Default::default()
        }))
    }

    fn group(master_key: &[u8], f: ChatFlags) -> Vec<u8> {
        wrap(Record::GroupV2(GroupV2Record {
            master_key: master_key.to_vec(),
            archived: f.archived,
            marked_unread: f.marked_unread,
            muted_until_timestamp: f.mute_until as u64,
            blocked: f.blocked,
            whitelisted: f.whitelisted,
            ..Default::default()
        }))
    }

    fn e164_pin() -> PinnedConversation {
        PinnedConversation {
            identifier: Some(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
                e164: "+1555".into(),
                ..Default::default()
            })),
        }
    }

    fn account(pins: Vec<PinnedConversation>, note_to_self: (bool, bool)) -> Vec<u8> {
        wrap(Record::Account(AccountRecord {
            pinned_conversations: pins,
            note_to_self_archived: note_to_self.0,
            note_to_self_marked_unread: note_to_self.1,
            ..Default::default()
        }))
    }

    fn pin_contact(aci: &str) -> PinnedConversation {
        PinnedConversation {
            identifier: Some(pinned_conversation::Identifier::Contact(pinned_conversation::Contact {
                service_id: aci.into(),
                ..Default::default()
            })),
        }
    }

    fn pin_group(master_key: &[u8]) -> PinnedConversation {
        PinnedConversation {
            identifier: Some(pinned_conversation::Identifier::GroupMasterKey(master_key.to_vec())),
        }
    }

    async fn db() -> ProjectionDb {
        ProjectionDb::open("sqlite::memory:", None).await.unwrap()
    }

    async fn flags_of(db: &ProjectionDb, id: &str) -> ChatFlags {
        db.chat_flags(id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{id} is not in the projection"))
    }

    async fn msg_in(db: &ProjectionDb, conv: &str, author: &str, t: i64) {
        db.insert_message(&crate::projection::Message {
            conversation_id: conv.into(),
            author: author.into(),
            sent_at: t,
            received_at: t,
            body: Some("x".into()),
            is_outgoing: false,
            status: crate::projection::MessageStatus::Read,
            edited: false,
            deleted: false,
            quote: None,
            kind: crate::projection::MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
        })
        .await
        .unwrap();
    }

    /// A projection that already applied manifest v3 = {A → alice, ACC → the account}.
    async fn synced_db() -> ProjectionDb {
        let db = db().await;
        msg_in(&db, &format!("c:{ALICE}"), ALICE, 1).await;
        db.replace_storage_items(&[
            StorageItemRow {
                key: key(0xA),
                kind: KIND_CONTACT,
                conversation_id: Some(format!("c:{ALICE}")),
            },
            StorageItemRow {
                key: key(0xAC),
                kind: KIND_ACCOUNT,
                conversation_id: None,
            },
        ])
        .await
        .unwrap();
        db.set_storage_state(&StorageState {
            version: 3,
            record_ikm: vec![],
            key_fingerprint: keys().fingerprint,
            account_dirty: false,
            unmappable_pins: vec![],
        })
        .await
        .unwrap();
        db
    }

    fn gkey() -> Vec<u8> {
        vec![0xCD; 32]
    }

    #[tokio::test]
    async fn first_pull_applies_flags_pins_and_records_the_manifest() {
        let db = db().await;
        msg_in(&db, &format!("c:{ALICE}"), ALICE, 1).await;
        let gid = format!("g:{}", hex::encode(gkey()));
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 3,
                record_ikm: vec![7; 32],
                identifiers: vec![
                    (key(0xA), KIND_CONTACT),
                    (key(0xB), KIND_GROUPV2),
                    (key(0xC), KIND_ACCOUNT),
                    (key(0xD), KIND_CHAT_FOLDER),
                ],
            }),
            ..Default::default()
        };
        api.items.insert(
            key(0xA),
            contact(
                ALICE,
                ChatFlags {
                    archived: true,
                    mute_until: i64::MAX,
                    ..Default::default()
                },
            ),
        );
        api.items.insert(
            key(0xB),
            group(
                &gkey(),
                ChatFlags {
                    marked_unread: true,
                    whitelisted: true,
                    ..Default::default()
                },
            ),
        );
        api.items.insert(
            key(0xC),
            account(vec![pin_group(&gkey()), e164_pin(), pin_contact(ALICE)], (false, true)),
        );
        api.items.insert(key(0xD), wrap(Record::ChatFolder(Default::default())));
        let out = pull(&mut api, &db, &keys(), ME).await.unwrap();
        assert_eq!((out.applied, out.chats_changed, out.version), (4, true, 3));
        let alice = db
            .conversations()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.id == format!("c:{ALICE}"))
            .unwrap();
        assert_eq!(
            (alice.archived, alice.mute_until, alice.pinned_order),
            (false, i64::MAX, Some(2)),
            "the pin wins over the archive"
        );
        assert!(db.has_conversation(&gid).await.unwrap());
        let g = flags_of(&db, &gid).await;
        assert!(g.marked_unread && g.whitelisted);
        let me = flags_of(&db, &format!("c:{ME}")).await;
        assert!(me.marked_unread && !me.archived);
        let rows = db.storage_items().await.unwrap();
        let mapping: Vec<(u8, Option<String>)> = rows.iter().map(|r| (r.key[0], r.conversation_id.clone())).collect();
        assert_eq!(
            mapping,
            vec![
                (0xA, Some(format!("c:{ALICE}"))),
                (0xB, Some(gid.clone())),
                (0xC, None),
                (0xD, None)
            ]
        );
        let state = db.storage_state().await.unwrap().unwrap();
        assert_eq!(
            (state.version, state.record_ikm.len(), state.key_fingerprint),
            (3, 32, keys().fingerprint)
        );
        assert_eq!(state.unmappable_pins, vec![e164_pin().encode_to_vec()]);
        assert!(db.dirty_chats().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unchanged_manifest_changes_nothing() {
        let db = synced_db().await;
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = pull(&mut api, &db, &keys(), ME).await.unwrap();
        assert_eq!((out.applied, out.chats_changed, out.version), (0, false, 3));
        assert!(api.reads.is_empty());
        assert_eq!(api.manifest_calls, vec![3]);
    }

    #[tokio::test]
    async fn only_new_keys_are_read_and_gone_keys_are_dropped() {
        let db = synced_db().await;
        db.set_mute_until(&format!("c:{ALICE}"), 5).await.unwrap(); // dirty, and stays so
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![(key(0xAC), KIND_ACCOUNT), (key(0xC), KIND_CONTACT)],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(key(0xC), contact(BOB, ChatFlags::default()));
        let out = pull(&mut api, &db, &keys(), ME).await.unwrap();
        assert_eq!((out.applied, out.version), (1, 4));
        assert_eq!(api.reads, vec![vec![key(0xC)]]);
        let mapping: Vec<(u8, Option<String>)> = db
            .storage_items()
            .await
            .unwrap()
            .iter()
            .map(|r| (r.key[0], r.conversation_id.clone()))
            .collect();
        assert_eq!(mapping, vec![(0xAC, None), (0xC, Some(format!("c:{BOB}")))]);
        let dirty = db.dirty_chats().await.unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(
            dirty[0].1.mute_until, 5,
            "alice's record wasn't re-read: her change stands"
        );
    }

    #[tokio::test]
    async fn a_rewritten_record_overrides_a_dirty_local_change() {
        let db = synced_db().await;
        db.set_mute_until(&format!("c:{ALICE}"), 9).await.unwrap();
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![(key(0xAC), KIND_ACCOUNT), (key(0xA2), KIND_CONTACT)],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(key(0xA2), contact(ALICE, ChatFlags::default()));
        pull(&mut api, &db, &keys(), ME).await.unwrap();
        assert_eq!(flags_of(&db, &format!("c:{ALICE}")).await.mute_until, 0);
        assert!(db.dirty_chats().await.unwrap().is_empty(), "remote wins");
        let keys_now: Vec<u8> = db.storage_items().await.unwrap().iter().map(|r| r.key[0]).collect();
        assert_eq!(keys_now, vec![0xAC, 0xA2]);
    }

    #[tokio::test]
    async fn a_key_the_server_does_not_return_is_dropped_not_fatal() {
        let db = synced_db().await;
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![
                    (key(0xAC), KIND_ACCOUNT),
                    (key(0xA), KIND_CONTACT),
                    (key(0xD), KIND_CONTACT),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = pull(&mut api, &db, &keys(), ME).await.unwrap();
        assert_eq!((out.applied, out.version), (0, 4));
        let keys_now: Vec<u8> = db.storage_items().await.unwrap().iter().map(|r| r.key[0]).collect();
        assert_eq!(keys_now, vec![0xAC, 0xA], "D is not ours to list");
    }

    #[tokio::test]
    async fn an_undecryptable_item_aborts_before_anything_is_written() {
        let db = synced_db().await;
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![
                    (key(0xAC), KIND_ACCOUNT),
                    (key(0xA), KIND_CONTACT),
                    (key(0xC), KIND_CONTACT),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.fail_items.insert(key(0xC));
        assert!(matches!(
            pull(&mut api, &db, &keys(), ME).await,
            Err(StorageError::Invalid(_))
        ));
        assert_eq!(db.storage_state().await.unwrap().unwrap().version, 3);
        assert_eq!(db.storage_items().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn garbage_in_an_item_aborts_too() {
        let db = synced_db().await;
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![
                    (key(0xAC), KIND_ACCOUNT),
                    (key(0xA), KIND_CONTACT),
                    (key(0xC), KIND_CONTACT),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(key(0xC), vec![0xFF, 0xFF, 0xFF]);
        assert!(matches!(
            pull(&mut api, &db, &keys(), ME).await,
            Err(StorageError::Invalid(_))
        ));
        assert_eq!(db.storage_state().await.unwrap().unwrap().version, 3);
    }

    #[tokio::test]
    async fn a_changed_storage_key_wipes_the_state_first_but_keeps_what_is_dirty() {
        let db = synced_db().await;
        db.set_account_dirty(true).await.unwrap();
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 9,
                identifiers: vec![(key(0x9A), KIND_ACCOUNT)],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(key(0x9A), account(vec![], (false, false)));
        let other = Keys {
            fingerprint: vec![2; 32],
        };
        let out = pull(&mut api, &db, &other, ME).await.unwrap();
        assert_eq!(api.manifest_calls, vec![0], "asked from nothing");
        assert_eq!(out.version, 9);
        let state = db.storage_state().await.unwrap().unwrap();
        assert_eq!(
            (state.version, state.key_fingerprint.clone(), state.account_dirty),
            (9, other.fingerprint, true)
        );
        let keys_now: Vec<u8> = db.storage_items().await.unwrap().iter().map(|r| r.key[0]).collect();
        assert_eq!(keys_now, vec![0x9A]);
    }

    #[tokio::test]
    async fn unauthorized_is_surfaced() {
        let db = synced_db().await;
        let mut api = FakeApi {
            unauthorized: true,
            ..Default::default()
        };
        assert!(matches!(
            pull(&mut api, &db, &keys(), ME).await,
            Err(StorageError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn blocked_and_whitelisted_land_in_the_projection() {
        let db = synced_db().await;
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 4,
                identifiers: vec![(key(0xAC), KIND_ACCOUNT), (key(0xA2), KIND_CONTACT)],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(
            key(0xA2),
            contact(
                ALICE,
                ChatFlags {
                    blocked: true,
                    whitelisted: true,
                    ..Default::default()
                },
            ),
        );
        pull(&mut api, &db, &keys(), ME).await.unwrap();
        let f = flags_of(&db, &format!("c:{ALICE}")).await;
        assert!(f.blocked && f.whitelisted);
    }
}
