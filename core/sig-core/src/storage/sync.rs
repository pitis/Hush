//! The sync itself: `pull` the phone's state into the projection, `push` our changes back, and
//! `run` = pull then push with conflict retries. Everything here is tested against a fake
//! `StorageApi` and an in-memory projection.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use crate::projection::{ChatFlags, ProjectionDb, StorageItemRow, StorageState};
use crate::storage::client::{Manifest, StorageApi, StorageError, Write};
use crate::storage::{records, Pin, Remote, KIND_ACCOUNT};

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

#[derive(Debug, PartialEq)]
pub(crate) struct PushOutcome {
    /// Records rewritten this time.
    pub written: usize,
    /// Dirty chats the phone has no record for: left dirty, tried again after every pull.
    pub skipped_no_record: usize,
}

/// Our pending changes into the Storage Service (spec: "Push"): every dirty chat's record is
/// read fresh, patched, and rewritten under a new key with the old one deleted; a pin change
/// (or Note to Self's flags) rewrites the account record the same way. One write at
/// `version + 1`; nothing is cleared, nothing we didn't replace is deleted.
pub(crate) async fn push(
    api: &mut impl StorageApi,
    db: &ProjectionDb,
    keys: &Keys,
    me: &str,
    device_id: u32,
) -> Result<PushOutcome, StorageError> {
    let nothing = PushOutcome {
        written: 0,
        skipped_no_record: 0,
    };
    if !db.storage_writes().await? {
        return Ok(nothing);
    }
    // never pulled, or pulled under another key: nothing to write against (a pull comes first)
    let Some(state) = db.storage_state().await? else {
        return Ok(nothing);
    };
    if state.key_fingerprint != keys.fingerprint {
        return Ok(nothing);
    }
    let me_id = format!("c:{me}");
    let mut dirty = db.dirty_chats().await?;
    // Note to Self has no record of its own: its flags live in the account record
    let mut account_dirty = state.account_dirty;
    let mut note_to_self_dirty = false;
    dirty.retain(|(id, _)| {
        if *id == me_id {
            account_dirty = true;
            note_to_self_dirty = true;
        }
        *id != me_id
    });
    if dirty.is_empty() && !account_dirty {
        return Ok(nothing);
    }
    let stored = db.storage_items().await?;
    let by_chat: HashMap<&str, &StorageItemRow> = stored
        .iter()
        .filter_map(|r| r.conversation_id.as_deref().map(|c| (c, r)))
        .collect();
    let mut skipped = 0;
    let mut targets: Vec<(Vec<u8>, String, ChatFlags)> = vec![];
    for (id, flags) in dirty {
        match by_chat.get(id.as_str()) {
            Some(row) => targets.push((row.key.clone(), id, flags)),
            None => skipped += 1,
        }
    }
    let account_key = match (account_dirty, stored.iter().find(|r| r.kind == KIND_ACCOUNT)) {
        (true, Some(row)) => Some(row.key.clone()),
        (true, None) => {
            tracing::warn!("the manifest has no account record: pins can't be written");
            None
        }
        (false, _) => None,
    };
    let to_read: Vec<Vec<u8>> = targets
        .iter()
        .map(|(k, _, _)| k.clone())
        .chain(account_key.iter().cloned())
        .collect();
    if to_read.is_empty() {
        return Ok(PushOutcome {
            written: 0,
            skipped_no_record: skipped,
        });
    }
    // read fresh: a record's other fields may have changed since the pull
    let current: HashMap<Vec<u8>, Vec<u8>> = api.read_items(&to_read, &state.record_ikm).await?.into_iter().collect();
    let mut rows = stored.clone();
    let mut inserts = vec![];
    let mut deletes = vec![];
    let mut written_chats = vec![];
    for (old, id, flags) in targets {
        let Some(plain) = current.get(&old) else {
            skipped += 1; // gone since the pull: the next pull sorts it out
            continue;
        };
        let patched = records::patch_chat(plain, &flags).map_err(StorageError::Invalid)?;
        let new_key = fresh_key();
        rekey(&mut rows, &old, &new_key);
        inserts.push((new_key, patched));
        deletes.push(old);
        written_chats.push(id);
    }
    let mut account_written = false;
    if let Some(old) = account_key {
        if let Some(plain) = current.get(&old) {
            let pins: Vec<Pin> = db
                .pinned_chat_ids()
                .await?
                .into_iter()
                .map(Pin::Chat)
                .chain(state.unmappable_pins.iter().cloned().map(Pin::Raw))
                .collect();
            let note = db
                .chat_flags(&me_id)
                .await?
                .map_or((false, false), |f| (f.archived, f.marked_unread));
            let patched = records::patch_account(plain, &pins, note).map_err(StorageError::Invalid)?;
            let new_key = fresh_key();
            rekey(&mut rows, &old, &new_key);
            inserts.push((new_key, patched));
            deletes.push(old);
            account_written = true;
        } else {
            tracing::warn!("the account record wasn't returned: pins wait for the next pull");
        }
    }
    if inserts.is_empty() {
        return Ok(PushOutcome {
            written: 0,
            skipped_no_record: skipped,
        });
    }
    let identifiers: Vec<(Vec<u8>, i32)> = rows.iter().map(|r| (r.key.clone(), r.kind)).collect();
    check_manifest(&identifiers, &deletes)?;
    let write = Write {
        manifest: Manifest {
            version: state.version + 1,
            record_ikm: state.record_ikm.clone(),
            identifiers,
        },
        source_device: device_id,
        inserts,
        deletes,
    };
    api.write(&write).await?;
    let written = write.inserts.len();
    db.replace_storage_items(&rows).await?;
    db.set_storage_state(&StorageState {
        version: state.version + 1,
        account_dirty: account_dirty && !account_written,
        ..state
    })
    .await?;
    if note_to_self_dirty && account_written {
        written_chats.push(me_id);
    }
    db.clear_dirty(&written_chats).await?;
    tracing::info!(version = write.manifest.version, written, skipped, "storage push");
    Ok(PushOutcome {
        written,
        skipped_no_record: skipped,
    })
}

fn fresh_key() -> Vec<u8> {
    rand::random::<[u8; 16]>().to_vec()
}

fn rekey(rows: &mut [StorageItemRow], old: &[u8], new: &[u8]) {
    if let Some(row) = rows.iter_mut().find(|r| r.key == old) {
        row.key = new.to_vec();
    }
}

/// What every manifest we write must hold: one account record, no key twice, and no deleted
/// key still listed. A violation is a bug on our side: better not to write at all.
fn check_manifest(identifiers: &[(Vec<u8>, i32)], deletes: &[Vec<u8>]) -> Result<(), StorageError> {
    let accounts = identifiers.iter().filter(|(_, k)| *k == KIND_ACCOUNT).count();
    let unique: HashSet<&Vec<u8>> = identifiers.iter().map(|(k, _)| k).collect();
    if accounts != 1 || unique.len() != identifiers.len() || deletes.iter().any(|d| unique.contains(d)) {
        return Err(StorageError::Invalid("our own manifest (not written)".into()));
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub(crate) struct RunOutcome {
    pub pulled: PullOutcome,
    pub pushed: Option<PushOutcome>,
}

/// Desktop's backoff between attempts after another device wrote first.
const CONFLICT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(5)];
const ATTEMPTS: usize = 3;

/// Pull, then push; a conflict pulls again and retries, three attempts in all (the third
/// refusal is reported, the dirty marks stay for the next trigger). `sleep` is the wait
/// between attempts, injected so tests don't.
pub(crate) async fn run<F, Fut>(
    api: &mut impl StorageApi,
    db: &ProjectionDb,
    keys: &Keys,
    me: &str,
    device_id: u32,
    sleep: F,
) -> Result<RunOutcome, StorageError>
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut pulled = pull(api, db, keys, me).await?;
    for attempt in 0..ATTEMPTS {
        match push(api, db, keys, me, device_id).await {
            Ok(pushed) => {
                return Ok(RunOutcome {
                    pulled,
                    pushed: Some(pushed),
                })
            }
            Err(StorageError::Conflict) if attempt + 1 < ATTEMPTS => {
                tracing::info!(attempt, "another device wrote first: pulling and trying again");
                sleep(CONFLICT_BACKOFF.get(attempt).copied().unwrap_or_default()).await;
                let again = pull(api, db, keys, me).await?;
                pulled.applied += again.applied;
                pulled.chats_changed |= again.chats_changed;
                pulled.version = again.version;
            }
            Err(e) => return Err(e),
        }
    }
    Err(StorageError::Conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ChatFlags, ProjectionDb, StorageItemRow, StorageState};
    use crate::storage::client::{Manifest, StorageApi, StorageError, Write};
    use crate::storage::KIND_ACCOUNT;
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
    const KIND_CONTACT: i32 = 1;
    const KIND_GROUPV2: i32 = 3;
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
        write_attempts: u32,
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
            self.write_attempts += 1;
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

    // ---- push ----

    /// A contact record with a field no client knows, so a rewrite can prove it kept it.
    fn contact_with_future_field(aci: &str) -> Vec<u8> {
        use crate::storage::records::wire;
        let raw = contact(aci, ChatFlags::default());
        let (_, inner) = wire::first_field(&raw).unwrap();
        let mut inner = inner.to_vec();
        wire::append_bytes(&mut inner, 900, b"future");
        let mut out = vec![];
        wire::append_bytes(&mut out, 1, &inner);
        out
    }

    /// The service holding exactly what `synced_db` applied (manifest v3: A → alice, AC → account).
    fn synced_api() -> FakeApi {
        let mut api = FakeApi {
            manifest: Some(Manifest {
                version: 3,
                identifiers: vec![(key(0xA), KIND_CONTACT), (key(0xAC), KIND_ACCOUNT)],
                ..Default::default()
            }),
            ..Default::default()
        };
        api.items.insert(key(0xA), contact_with_future_field(ALICE));
        api.items
            .insert(key(0xAC), account(vec![pin_group(&gkey())], (false, false)));
        api
    }

    fn no_sleep(_: std::time::Duration) -> std::future::Ready<()> {
        std::future::ready(())
    }

    #[tokio::test]
    async fn push_rewrites_only_dirty_records_under_new_keys() {
        let db = synced_db().await;
        msg_in(&db, &format!("c:{BOB}"), BOB, 2).await;
        db.replace_storage_items(&[
            StorageItemRow {
                key: key(0xA),
                kind: KIND_CONTACT,
                conversation_id: Some(format!("c:{ALICE}")),
            },
            StorageItemRow {
                key: key(0xB),
                kind: KIND_CONTACT,
                conversation_id: Some(format!("c:{BOB}")),
            },
            StorageItemRow {
                key: key(0xAC),
                kind: KIND_ACCOUNT,
                conversation_id: None,
            },
        ])
        .await
        .unwrap();
        let mut api = synced_api();
        api.manifest
            .as_mut()
            .unwrap()
            .identifiers
            .push((key(0xB), KIND_CONTACT));
        api.items.insert(key(0xB), contact(BOB, ChatFlags::default()));
        db.set_mute_until(&format!("c:{ALICE}"), 42).await.unwrap();
        let out = push(&mut api, &db, &keys(), ME, 3).await.unwrap();
        assert_eq!((out.written, out.skipped_no_record), (1, 0));
        assert_eq!(api.writes.len(), 1);
        let w = &api.writes[0];
        assert_eq!((w.manifest.version, w.source_device), (4, 3));
        assert_eq!(w.deletes, vec![key(0xA)]);
        assert_eq!(w.inserts.len(), 1);
        let (new_key, plain) = &w.inserts[0];
        assert_eq!(new_key.len(), 16);
        assert_ne!(new_key, &key(0xA));
        let Remote::Chat { conversation_id, flags } = records::decode(plain).unwrap() else {
            panic!()
        };
        assert_eq!((conversation_id, flags.mute_until), (format!("c:{ALICE}"), 42));
        assert!(
            plain.windows(6).any(|x| x == b"future"),
            "the unknown field survived the rewrite"
        );
        let mut ids = w.manifest.identifiers.clone();
        ids.sort();
        let mut expected = vec![
            (key(0xB), KIND_CONTACT),
            (key(0xAC), KIND_ACCOUNT),
            (new_key.clone(), KIND_CONTACT),
        ];
        expected.sort();
        assert_eq!(ids, expected);
        // the projection moved on with it
        let rows = db.storage_items().await.unwrap();
        assert!(rows
            .iter()
            .any(|r| &r.key == new_key && r.conversation_id.as_deref() == Some(&format!("c:{ALICE}"))));
        assert!(!rows.iter().any(|r| r.key == key(0xA)));
        assert_eq!(db.storage_state().await.unwrap().unwrap().version, 4);
        assert!(db.dirty_chats().await.unwrap().is_empty());
        // nothing dirty: nothing sent
        let again = push(&mut api, &db, &keys(), ME, 3).await.unwrap();
        assert_eq!((again.written, api.writes.len()), (0, 1));
    }

    #[tokio::test]
    async fn the_account_is_rewritten_with_local_pins_kept_raw_entries_and_note_to_self() {
        let db = synced_db().await;
        msg_in(&db, &format!("c:{ME}"), ME, 2).await;
        db.set_storage_state(&StorageState {
            version: 3,
            record_ikm: vec![],
            key_fingerprint: keys().fingerprint,
            account_dirty: false,
            unmappable_pins: vec![e164_pin().encode_to_vec()],
        })
        .await
        .unwrap();
        db.set_pinned(&format!("c:{ALICE}"), true).await.unwrap(); // the account, and alice's record
        db.set_marked_unread(&format!("c:{ME}"), true).await.unwrap(); // Note to Self lives in the account too
        let mut api = synced_api();
        let out = push(&mut api, &db, &keys(), ME, 1).await.unwrap();
        assert_eq!((out.written, out.skipped_no_record), (2, 0));
        let w = &api.writes[0];
        assert_eq!(w.deletes.len(), 2);
        assert!(w.deletes.contains(&key(0xAC)) && w.deletes.contains(&key(0xA)));
        let account_plain = w
            .inserts
            .iter()
            .find_map(|(_, p)| match records::decode(p).unwrap() {
                Remote::Account { pins, note_to_self } => Some((pins, note_to_self)),
                _ => None,
            })
            .expect("the account was rewritten");
        assert_eq!(
            account_plain,
            (
                vec![Pin::Chat(format!("c:{ALICE}")), Pin::Raw(e164_pin().encode_to_vec())],
                (false, true)
            )
        );
        let state = db.storage_state().await.unwrap().unwrap();
        assert!(!state.account_dirty);
        assert_eq!(state.version, 4);
        assert!(
            db.dirty_chats().await.unwrap().is_empty(),
            "Note to Self's mark went with the account"
        );
    }

    #[tokio::test]
    async fn a_dirty_chat_without_a_record_stays_dirty_and_is_counted() {
        let db = synced_db().await;
        let carol = "c:44444444-4444-4444-4444-444444444444";
        msg_in(&db, carol, "44444444-4444-4444-4444-444444444444", 2).await;
        db.set_mute_until(carol, 1).await.unwrap();
        let mut api = synced_api();
        let out = push(&mut api, &db, &keys(), ME, 1).await.unwrap();
        assert_eq!((out.written, out.skipped_no_record), (0, 1));
        assert!(api.writes.is_empty());
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn writes_off_means_no_write() {
        let db = synced_db().await;
        db.set_storage_writes(false).await.unwrap();
        db.set_mute_until(&format!("c:{ALICE}"), 1).await.unwrap();
        let mut api = synced_api();
        let out = push(&mut api, &db, &keys(), ME, 1).await.unwrap();
        assert_eq!(out.written, 0);
        assert!(api.writes.is_empty() && api.reads.is_empty());
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_conflict_pulls_and_retries_at_the_new_version() {
        let db = synced_db().await;
        db.set_mute_until(&format!("c:{ALICE}"), 7).await.unwrap();
        let mut api = synced_api();
        api.conflicts_left = 1;
        let out = run(&mut api, &db, &keys(), ME, 1, no_sleep).await.unwrap();
        assert_eq!(out.pushed.as_ref().map(|p| p.written), Some(1));
        assert_eq!((api.write_attempts, api.writes.len()), (2, 1));
        let w = &api.writes[0];
        assert_eq!(w.manifest.version, 5, "the other device wrote 4; ours is 5");
        assert!(
            w.manifest.identifiers.iter().any(|(k, _)| k == &vec![0xE0; 16]),
            "the record pulled in between is listed"
        );
        assert!(db.has_conversation(&format!("c:{BOB}")).await.unwrap());
        assert!(db.dirty_chats().await.unwrap().is_empty());
        assert_eq!(db.storage_state().await.unwrap().unwrap().version, 5);
    }

    #[tokio::test]
    async fn three_conflicts_give_up_and_keep_the_marks() {
        let db = synced_db().await;
        db.set_mute_until(&format!("c:{ALICE}"), 7).await.unwrap();
        let mut api = synced_api();
        api.conflicts_left = 3;
        assert!(matches!(
            run(&mut api, &db, &keys(), ME, 1, no_sleep).await,
            Err(StorageError::Conflict)
        ));
        assert_eq!((api.write_attempts, api.writes.len()), (3, 0));
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1, "still to write");
        assert_eq!(
            db.storage_state().await.unwrap().unwrap().version,
            5,
            "pulled after each refusal"
        );
    }

    #[tokio::test]
    async fn run_pulls_then_pushes() {
        let db = synced_db().await;
        db.set_mute_until(&format!("c:{ALICE}"), 1).await.unwrap();
        let mut api = synced_api();
        let m = api.manifest.as_mut().unwrap();
        m.version = 4;
        m.identifiers.push((key(0xB), KIND_CONTACT));
        api.items.insert(key(0xB), contact(BOB, ChatFlags::default()));
        let out = run(&mut api, &db, &keys(), ME, 1, no_sleep).await.unwrap();
        assert_eq!((out.pulled.applied, out.pulled.version), (1, 4));
        assert_eq!(out.pushed.map(|p| p.written), Some(1));
        assert_eq!(api.writes[0].manifest.version, 5);
        assert!(db.has_conversation(&format!("c:{BOB}")).await.unwrap());
    }

    #[tokio::test]
    async fn run_without_anything_dirty_pushes_nothing() {
        let db = synced_db().await;
        let mut api = synced_api();
        let out = run(&mut api, &db, &keys(), ME, 1, no_sleep).await.unwrap();
        assert_eq!(out.pushed.map(|p| p.written), Some(0));
        assert_eq!(api.write_attempts, 0);
    }
}
