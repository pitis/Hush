# Storage Service sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The four chat flags (archived, pinned, muted, marked unread) follow the phone through Signal's Storage Service in both directions, and `blocked`/`whitelisted` are read into the projection.

**Architecture:** A new `core/sig-core/src/storage/` module: `client.rs` (HTTP over presage's `PushService`, behind a `StorageApi` trait), `records.rs` (decode records; patch raw record bytes so unknown fields survive), `sync.rs` (`pull`, `push`, and `run` = pull-then-push with conflict retries), wired into the actor as a single-flight local task triggered by connect, the phone's `FetchLatest` sync message, and local flag changes. State lives in the projection (`storage_sync`, `storage_items`, dirty marks on `conversations`).

**Tech Stack:** Rust (tokio current-thread, sqlx/SQLCipher, prost via `presage::libsignal_service::proto`, `StorageService::{encrypt,decrypt}_{manifest,item}` and `StorageServiceKey::from_master_key` from libsignal-service, `sha2`, `rand`), Swift (one Settings toggle).

**Spec:** `docs/superpowers/specs/2026-10-16-m4-storage-sync-design.md`

## Global Constraints

- presage pinned at 971b14b, libsignal-service fb92b13: no bumps. The store is `!Send`: `fetch_master_key` only on the actor thread.
- Never log record contents, names, keys or e164s: counts, record types, item-key hex prefixes (4 bytes) and errors only.
- Never `clearAll`; never delete an identifier we didn't replace; never create a record for a chat that has none; manifest `recordIkm` unchanged.
- Writes only when `storage_writes` is on (absent = on); reads always.
- `make format && make lint` clean; tests first; one PR, `Part of #20`.

## Review Focus

1. A record rewritten by the phone (new key, same chat) while a local change is dirty: the pull must apply the remote values and clear the dirty mark (remote wins), never leave the chat dirty with stale local values. Test in Task 4.
2. A pinned entry we can't map (e164-only contact): the next push must carry it unchanged in the pinned list, after our own pins. Test in Task 5.
3. An item key listed in the manifest that the server doesn't return: must be dropped from our set without aborting, and never land in `deleteKey`. Test in Task 4.
4. A 409 whose pull brings a new version: the retry must write at the *new* version + 1 with the refreshed identifier set, and dirty marks must survive three strikes. Test in Task 5.
5. The storage key changing under us (new `Keys` sync message): the fingerprint check must wipe `storage_sync` and `storage_items` before pulling, or every item fails to decrypt forever. Test in Task 4.

---

### Task 1: Projection state for the sync (migration 0011 + db API)

**Files:**
- Create: `core/sig-core/migrations/0011_storage_sync.sql`
- Modify: `core/sig-core/src/projection/model.rs` (Conversation: `blocked`, `whitelisted`)
- Modify: `core/sig-core/src/projection/db.rs` (row mapping, setters mark dirty, new methods + tests)
- Modify: `app/SemaphoreTests/FakeClient.swift` (`conversation()` fixture gains the two fields; done in Task 7 with the bindings)

**Interfaces:**
- Produces (all on `ProjectionDb`, `Result<_, sqlx::Error>`):
  - `pub struct StorageState { pub version: u64, pub record_ikm: Vec<u8>, pub key_fingerprint: Vec<u8>, pub account_dirty: bool, pub unmappable_pins: Vec<Vec<u8>> }` (in `projection/model.rs`, `#[derive(Clone, Debug, PartialEq, Default)]`; `unmappable_pins` stored as one BLOB: length-prefixed entries, `u32 LE len || bytes`).
  - `pub struct StorageItemRow { pub key: Vec<u8>, pub kind: i32, pub conversation_id: Option<String> }`.
  - `pub struct ChatFlags { pub archived: bool, pub marked_unread: bool, pub mute_until: i64, pub blocked: bool, pub whitelisted: bool }`.
  - `async fn storage_state(&self) -> Result<Option<StorageState>>`, `async fn set_storage_state(&self, s: &StorageState)`, `async fn reset_storage_state(&self)` (deletes `storage_sync` and `storage_items`).
  - `async fn storage_items(&self) -> Result<Vec<StorageItemRow>>`, `async fn replace_storage_items(&self, rows: &[StorageItemRow])` (delete all, insert all, one transaction).
  - `async fn apply_remote_chat(&self, id: &str, flags: &ChatFlags)`: `INSERT OR IGNORE INTO conversations(id, kind)` then `UPDATE … SET archived, marked_unread, mute_until, blocked, whitelisted, storage_dirty = 0, pinned_order = CASE WHEN ?archived THEN NULL ELSE pinned_order END`. Returns `bool` (whether any value changed).
  - `async fn apply_remote_pins(&self, ids: &[String])`: one transaction: `UPDATE conversations SET pinned_order = NULL`; then for `(i, id)` → `INSERT OR IGNORE` the chat, `UPDATE SET pinned_order = i + 1, archived = 0`.
  - `async fn dirty_chats(&self) -> Result<Vec<(String, ChatFlags)>>` (rows with `storage_dirty = 1`), `async fn clear_dirty(&self, ids: &[String])`.
  - `async fn set_account_dirty(&self, dirty: bool)` (creates the `storage_sync` row with version 0 if missing).
  - `async fn storage_writes(&self) -> Result<bool>` (settings key `storage_writes`, absent = true), `async fn set_storage_writes(&self, on: bool)`.
  - The existing `set_archived`, `set_mute_until`, `set_marked_unread` also set `storage_dirty = 1`; `set_pinned` sets `account_dirty = 1` (and, when pinning unarchives, `storage_dirty = 1` on that chat). `import_history` leaves dirty marks alone.

- [ ] **Step 1: Write the failing tests** (in `db.rs` `mod tests`, after `a_chat_is_muted_while_its_time_has_not_come`)

```rust
    #[tokio::test]
    async fn storage_state_round_trips_and_resets() {
        let db = db().await;
        assert_eq!(db.storage_state().await.unwrap(), None);
        let s = StorageState {
            version: 7,
            record_ikm: vec![1; 32],
            key_fingerprint: vec![2; 32],
            account_dirty: true,
            unmappable_pins: vec![vec![9, 9], vec![]],
        };
        db.set_storage_state(&s).await.unwrap();
        assert_eq!(db.storage_state().await.unwrap(), Some(s.clone()));
        db.replace_storage_items(&[StorageItemRow { key: vec![1; 16], kind: 4, conversation_id: None }])
            .await
            .unwrap();
        db.reset_storage_state().await.unwrap();
        assert_eq!(db.storage_state().await.unwrap(), None);
        assert!(db.storage_items().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn storage_items_are_replaced_as_a_set() {
        let db = db().await;
        let row = |k: u8, kind, conv: Option<&str>| StorageItemRow {
            key: vec![k; 16],
            kind,
            conversation_id: conv.map(String::from),
        };
        db.replace_storage_items(&[row(1, 1, Some("c:alice")), row(2, 4, None)]).await.unwrap();
        db.replace_storage_items(&[row(3, 3, Some("g:ab"))]).await.unwrap();
        let keys: Vec<u8> = db.storage_items().await.unwrap().iter().map(|r| r.key[0]).collect();
        assert_eq!(keys, vec![3]);
    }

    #[tokio::test]
    async fn remote_flags_apply_to_a_chat_and_create_it_if_needed() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.set_archived("c:alice", true).await.unwrap(); // dirty now
        let flags = ChatFlags { archived: false, marked_unread: true, mute_until: 5, blocked: true, whitelisted: true };
        assert!(db.apply_remote_chat("c:alice", &flags).await.unwrap());
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.archived, c.marked_unread, c.mute_until, c.blocked, c.whitelisted), (false, true, 5, true, true));
        assert!(db.dirty_chats().await.unwrap().is_empty(), "remote wins and clears the mark");
        assert!(!db.apply_remote_chat("c:alice", &flags).await.unwrap(), "nothing changed the second time");
        // a chat we have no messages with: created, not listed
        db.apply_remote_chat("c:bob", &ChatFlags { archived: true, ..Default::default() }).await.unwrap();
        assert!(db.has_conversation("c:bob").await.unwrap());
        assert!(db.conversations().await.unwrap().iter().all(|c| c.id != "c:bob"));
        // a remote archive unpins
        db.set_pinned("c:alice", true).await.unwrap();
        db.apply_remote_chat("c:alice", &ChatFlags { archived: true, ..Default::default() }).await.unwrap();
        assert_eq!(conv(&db, "c:alice").await.pinned_order, None);
    }

    #[tokio::test]
    async fn remote_pins_replace_local_ones_in_order() {
        let db = db().await;
        for (c, a, t) in [("c:alice", "alice", 1), ("c:bob", "bob", 2), ("c:carol", "carol", 3)] {
            db.insert_message(&in_conv(c, msg(a, t, "x"))).await.unwrap();
        }
        db.set_pinned("c:alice", true).await.unwrap();
        db.set_archived("c:carol", true).await.unwrap();
        db.apply_remote_pins(&["c:carol".into(), "c:bob".into(), "g:ff".into()]).await.unwrap();
        let order = |id: &str| futures::executor::block_on(conv(&db, id)).pinned_order; // or inline awaits
        assert_eq!(conv(&db, "c:alice").await.pinned_order, None);
        assert_eq!(conv(&db, "c:carol").await.pinned_order, Some(1));
        assert!(!conv(&db, "c:carol").await.archived, "a remote pin unarchives");
        assert_eq!(conv(&db, "c:bob").await.pinned_order, Some(2));
        assert!(db.has_conversation("g:ff").await.unwrap(), "an unknown pinned group is created");
        let _ = order;
    }

    #[tokio::test]
    async fn local_flag_changes_mark_the_chat_or_the_account_dirty() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        assert!(db.dirty_chats().await.unwrap().is_empty());
        db.set_mute_until("c:alice", 9).await.unwrap();
        let dirty = db.dirty_chats().await.unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!((dirty[0].0.as_str(), dirty[0].1.mute_until), ("c:alice", 9));
        db.clear_dirty(&["c:alice".into()]).await.unwrap();
        assert!(db.dirty_chats().await.unwrap().is_empty());
        assert_eq!(db.storage_state().await.unwrap(), None);
        db.set_pinned("c:alice", true).await.unwrap();
        assert!(db.storage_state().await.unwrap().unwrap().account_dirty);
        db.set_account_dirty(false).await.unwrap();
        assert!(!db.storage_state().await.unwrap().unwrap().account_dirty);
        db.set_archived("c:alice", true).await.unwrap(); // archiving unpins: the account changes too
        assert!(db.storage_state().await.unwrap().unwrap().account_dirty);
    }

    #[tokio::test]
    async fn storage_writes_are_on_unless_turned_off() {
        let db = db().await;
        assert!(db.storage_writes().await.unwrap());
        db.set_storage_writes(false).await.unwrap();
        assert!(!db.storage_writes().await.unwrap());
        db.set_storage_writes(true).await.unwrap();
        assert!(db.storage_writes().await.unwrap());
    }
```
(Drop the `order` closure line in `remote_pins_replace_local_ones_in_order`; it is only there to show intent. Use `..Default::default()` on `ChatFlags` — derive `Default`.)

- [ ] **Step 2: Run them, expect compile failures** for `StorageState`, `StorageItemRow`, `ChatFlags`, `storage_state`, `apply_remote_chat`, … (`cargo test -p sig-core --lib storage_` → `error[E0433]`/`E0599`).

- [ ] **Step 3: Migration**

```sql
-- Storage Service sync: the manifest we applied, its items, and what is waiting to be written.
CREATE TABLE storage_sync (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL DEFAULT 0,
  record_ikm BLOB NOT NULL DEFAULT X'',
  key_fingerprint BLOB NOT NULL DEFAULT X'',
  account_dirty INTEGER NOT NULL DEFAULT 0 CHECK (account_dirty IN (0, 1)),
  -- pinned-list entries we couldn't map, as raw PinnedConversation bytes: u32 LE length || bytes…
  unmappable_pins BLOB NOT NULL DEFAULT X''
);
-- Every identifier of the applied manifest: 16-byte key, ManifestRecord.Identifier.Type, and the
-- chat it maps to when we understood the record.
CREATE TABLE storage_items (
  key BLOB PRIMARY KEY CHECK (length(key) = 16),
  kind INTEGER NOT NULL,
  conversation_id TEXT REFERENCES conversations(id)
);
ALTER TABLE conversations ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1));
ALTER TABLE conversations ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0 CHECK (whitelisted IN (0, 1));
-- A local flag change not yet written to the Storage Service.
ALTER TABLE conversations ADD COLUMN storage_dirty INTEGER NOT NULL DEFAULT 0 CHECK (storage_dirty IN (0, 1));
```

- [ ] **Step 4: Model and db methods.** Add `blocked`/`whitelisted` to `Conversation` (doc: "Blocked / message request accepted, as the phone says; read-only here") and to `row_to_conversation`. Add the three structs to `model.rs` (re-export from `projection/mod.rs` like the others). Implement the methods with the SQL in the Interfaces block; `storage_state` reads `SELECT … FROM storage_sync WHERE id = 1` with `fetch_optional`; `set_storage_state` is `INSERT … ON CONFLICT(id) DO UPDATE SET` every column; `set_account_dirty` is `INSERT INTO storage_sync(id, account_dirty) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET account_dirty = excluded.account_dirty`. Encode `unmappable_pins` with two private fns `pack_blobs(&[Vec<u8>]) -> Vec<u8>` / `unpack_blobs(&[u8]) -> Vec<Vec<u8>>`. Mark dirty in the existing setters:
  - `set_archived`: `… SET archived = ?2, pinned_order = CASE WHEN ?2 THEN NULL ELSE pinned_order END, storage_dirty = 1 …`, then if `archived` and the row had a pin → `set_account_dirty(true)` (do it unconditionally when archiving: one cheap upsert).
  - `set_pinned`: after a successful change → `set_account_dirty(true)`; when pinning also `storage_dirty = 1` in the same UPDATE (archived may have flipped).
  - `set_mute_until`, `set_marked_unread`: `storage_dirty = 1`.
  - `clear_marked_unread` (opening a chat): also `storage_dirty = 1` when it changed — the phone should learn the chat was read.

- [ ] **Step 5: Run the tests** (`cargo test -p sig-core --lib`). Expected: all pass (the earlier chat-flag tests still pass; `chat_flags_announce_the_list_and_refuse_bad_ids` unaffected).

- [ ] **Step 6: Commit** — `git add core && git commit -m "The projection keeps the Storage Service state and marks chat flag changes to write"`.

---

### Task 2: Records: decode what we keep, patch raw bytes for writes

**Files:**
- Create: `core/sig-core/src/storage/mod.rs` (module decl, shared types), `core/sig-core/src/storage/records.rs`
- Modify: `core/sig-core/src/lib.rs` (`pub mod storage;`), `core/sig-core/Cargo.toml` (`sha2 = "0.10"` and `rand = "0.9"` only if not already present — check `cargo tree -e normal -i sha2` / `rand`; `prost` is reachable as `presage::libsignal_service::prelude::ProtobufMessage`)

**Interfaces:**
- Produces, in `storage/mod.rs`:
  - `pub(crate) const KIND_CONTACT: i32 = 1; KIND_GROUPV2: i32 = 3; KIND_ACCOUNT: i32 = 4;` (from `manifest_record::identifier::Type`).
  - `pub(crate) enum Remote { Chat { conversation_id: String, flags: ChatFlags }, Account { pins: Vec<Pin>, note_to_self: (bool, bool) }, Other }` with `pub(crate) enum Pin { Chat(String), Raw(Vec<u8>) }` (Raw = the encoded `PinnedConversation` we couldn't map, kept verbatim).
- Produces, in `records.rs`:
  - `pub(crate) fn decode(plaintext: &[u8]) -> Result<Remote, String>` — `StorageRecord::decode`; `Contact` → aci from `aci` (string) or `aci_binary` (16 bytes → UUID hyphenated lowercase); no ACI → `Other`; `GroupV2` → `g:<hex master_key>` (32 bytes else `Other`); `Account` → pins: `Contact{service_id|service_id_binary}` → `Pin::Chat("c:<aci>")` when it parses as a UUID (a `PNI:` prefix or e164-only → `Raw`), `GroupMasterKey(32 bytes)` → `Pin::Chat("g:<hex>")`, everything else → `Raw(entry.encode_to_vec())`; `note_to_self = (note_to_self_archived, note_to_self_marked_unread)`. `mute_until = i64::try_from(muted_until_timestamp).unwrap_or(i64::MAX)`.
  - `pub(crate) fn patch_chat(plaintext: &[u8], flags: &ChatFlags) -> Result<Vec<u8>, String>` — the plaintext is a `StorageRecord`; find its single top-level field (1 = contact: fields archived 11, markedUnread 12, mutedUntilTimestamp 13; 3 = groupV2: 4, 5, 6); rebuild: outer tag + length + (inner bytes with those field numbers stripped + the three appended, varint-encoded: bool as 0/1, mute as u64 (i64::MAX → 2^63−1)). Anything else (other outer field, several outer fields) → `Err`. `blocked`/`whitelisted` are never written.
  - `pub(crate) fn patch_account(plaintext: &[u8], pins: &[Pin], note_to_self: (bool, bool), me: &str) -> Result<Vec<u8>, String>` — outer field 4; strip 14 (pinnedConversations), 5 (noteToSelfArchived), 10 (noteToSelfMarkedUnread); append one field-14 entry per pin (`Pin::Chat("c:<aci>")` → `PinnedConversation { identifier: Contact { service_id: aci, .. } }`; `Pin::Chat("g:<hex>")` → `GroupMasterKey(bytes)`; `Pin::Raw(b)` → `b` as-is), then 5 and 10 as varints (only appended when true; false = absent, same meaning).
  - `pub(crate) mod wire` with `pub fn strip_fields(msg: &[u8], fields: &[u32]) -> Result<Vec<u8>, String>` and `pub fn append_varint(out: &mut Vec<u8>, field: u32, v: u64)`, `pub fn append_bytes(out: &mut Vec<u8>, field: u32, b: &[u8])`, `pub fn first_field(msg: &[u8]) -> Result<(u32, &[u8]), String>` (the one length-delimited top-level field, error if the message has more than one field). The walker handles wire types 0 (varint), 1 (64-bit), 2 (length-delimited), 5 (32-bit); groups (3/4) → `Err("unsupported wire type")`.

- [ ] **Step 1: Write the failing tests** (`records.rs`, `#[cfg(test)] mod tests`):

```rust
    use super::*;
    use presage::libsignal_service::prelude::ProtobufMessage as _;
    use presage::libsignal_service::proto::{
        account_record::{pinned_conversation, PinnedConversation},
        storage_record::Record, AccountRecord, ContactRecord, GroupV2Record, StorageRecord,
    };

    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    fn contact(aci: &str) -> Vec<u8> {
        StorageRecord {
            record: Some(Record::Contact(ContactRecord {
                aci: aci.into(),
                archived: true,
                muted_until_timestamp: u64::MAX,
                blocked: true,
                whitelisted: false,
                given_name: "A".into(),
                ..Default::default()
            })),
        }
        .encode_to_vec()
    }

    #[test]
    fn a_contact_record_gives_the_chat_and_its_flags() {
        let Remote::Chat { conversation_id, flags } = decode(&contact(ALICE)).unwrap() else { panic!() };
        assert_eq!(conversation_id, format!("c:{ALICE}"));
        assert_eq!((flags.archived, flags.marked_unread, flags.mute_until, flags.blocked, flags.whitelisted),
                   (true, false, i64::MAX, true, false));
    }

    #[test]
    fn a_binary_aci_is_read_and_a_contact_without_aci_is_not_ours() {
        let bin = StorageRecord { record: Some(Record::Contact(ContactRecord {
            aci_binary: uuid::Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec(), ..Default::default() })) }.encode_to_vec();
        assert!(matches!(decode(&bin).unwrap(), Remote::Chat { conversation_id, .. } if conversation_id == format!("c:{ALICE}")));
        let pni_only = StorageRecord { record: Some(Record::Contact(ContactRecord { pni: "x".into(), ..Default::default() })) }.encode_to_vec();
        assert!(matches!(decode(&pni_only).unwrap(), Remote::Other));
    }

    #[test]
    fn a_group_record_gives_the_group_by_master_key() {
        let g = StorageRecord { record: Some(Record::GroupV2(GroupV2Record {
            master_key: vec![0xAB; 32], marked_unread: true, whitelisted: true, ..Default::default() })) }.encode_to_vec();
        let Remote::Chat { conversation_id, flags } = decode(&g).unwrap() else { panic!() };
        assert_eq!(conversation_id, format!("g:{}", "ab".repeat(32)));
        assert!(flags.marked_unread && flags.whitelisted && !flags.archived);
        let short = StorageRecord { record: Some(Record::GroupV2(GroupV2Record { master_key: vec![1; 4], ..Default::default() })) }.encode_to_vec();
        assert!(matches!(decode(&short).unwrap(), Remote::Other));
    }

    #[test]
    fn the_account_record_gives_pins_in_order_and_keeps_what_it_cannot_map() {
        let pin = |id| PinnedConversation { identifier: Some(id) };
        let e164 = pin(pinned_conversation::Identifier::Contact(pinned_conversation::Contact { e164: "+1555".into(), ..Default::default() }));
        let acct = StorageRecord { record: Some(Record::Account(AccountRecord {
            pinned_conversations: vec![
                pin(pinned_conversation::Identifier::GroupMasterKey(vec![0xCD; 32])),
                e164.clone(),
                pin(pinned_conversation::Identifier::Contact(pinned_conversation::Contact { service_id: ALICE.into(), ..Default::default() })),
            ],
            note_to_self_archived: true,
            ..Default::default() })) }.encode_to_vec();
        let Remote::Account { pins, note_to_self } = decode(&acct).unwrap() else { panic!() };
        assert_eq!(note_to_self, (true, false));
        assert!(matches!(&pins[0], Pin::Chat(id) if id == &format!("g:{}", "cd".repeat(32))));
        assert!(matches!(&pins[1], Pin::Raw(b) if b == &e164.encode_to_vec()));
        assert!(matches!(&pins[2], Pin::Chat(id) if id == &format!("c:{ALICE}")));
    }

    #[test]
    fn unknown_record_types_and_garbage_are_other_or_errors() {
        let folder = StorageRecord { record: Some(Record::ChatFolder(Default::default())) }.encode_to_vec();
        assert!(matches!(decode(&folder).unwrap(), Remote::Other));
        assert!(decode(&[0xFF, 0xFF, 0xFF]).is_err());
    }

    #[test]
    fn patching_a_contact_keeps_every_other_byte_including_unknown_fields() {
        // a field number no client knows (900), plus a known one we don't own (givenName)
        let mut raw = contact(ALICE);
        let (_, inner) = wire::first_field(&raw).unwrap();
        let mut inner = inner.to_vec();
        wire::append_bytes(&mut inner, 900, b"future");
        raw.clear();
        wire::append_bytes(&mut raw, 1, &inner);
        let patched = patch_chat(&raw, &ChatFlags { archived: false, marked_unread: true, mute_until: 1_000, blocked: false, whitelisted: true }).unwrap();
        let rec = StorageRecord::decode(&*patched).unwrap();
        let Some(Record::Contact(c)) = rec.record else { panic!() };
        assert_eq!((c.archived, c.marked_unread, c.muted_until_timestamp, c.given_name.as_str(), c.blocked), (false, true, 1_000, "A", true), "blocked is never written");
        let (_, inner2) = wire::first_field(&patched).unwrap();
        assert!(inner2.windows(6).any(|w| w == b"future"), "the unknown field survived");
        // idempotent on the owned fields: one occurrence each
        let stripped = wire::strip_fields(inner2, &[11, 12, 13]).unwrap();
        assert_eq!(stripped.len(), inner2.len() - (2 + 2 + 3), "exactly one bool, one bool, one 2-byte varint were present");
    }

    #[test]
    fn patching_a_group_and_refusing_other_records() {
        let g = StorageRecord { record: Some(Record::GroupV2(GroupV2Record { master_key: vec![1; 32], archived: true, ..Default::default() })) }.encode_to_vec();
        let p = patch_chat(&g, &ChatFlags { mute_until: i64::MAX, ..Default::default() }).unwrap();
        let Some(Record::GroupV2(g2)) = StorageRecord::decode(&*p).unwrap().record else { panic!() };
        assert_eq!((g2.archived, g2.muted_until_timestamp, g2.master_key.len()), (false, i64::MAX as u64, 32));
        let acct = StorageRecord { record: Some(Record::Account(Default::default())) }.encode_to_vec();
        assert!(patch_chat(&acct, &ChatFlags::default()).is_err());
    }

    #[test]
    fn patching_the_account_rewrites_pins_and_note_to_self_only() {
        let acct = StorageRecord { record: Some(Record::Account(AccountRecord {
            pinned_conversations: vec![PinnedConversation { identifier: Some(pinned_conversation::Identifier::GroupMasterKey(vec![9; 32])) }],
            note_to_self_archived: true, read_receipts: true, username: "u".into(), ..Default::default() })) }.encode_to_vec();
        let raw_pin = PinnedConversation { identifier: Some(pinned_conversation::Identifier::Contact(pinned_conversation::Contact { e164: "+1".into(), ..Default::default() })) }.encode_to_vec();
        let p = patch_account(&acct, &[Pin::Chat(format!("c:{ALICE}")), Pin::Chat(format!("g:{}", "ab".repeat(32))), Pin::Raw(raw_pin.clone())], (false, true), ALICE).unwrap();
        let Some(Record::Account(a)) = StorageRecord::decode(&*p).unwrap().record else { panic!() };
        assert_eq!(a.pinned_conversations.len(), 3);
        assert!(matches!(&a.pinned_conversations[0].identifier, Some(pinned_conversation::Identifier::Contact(c)) if c.service_id == ALICE));
        assert!(matches!(&a.pinned_conversations[1].identifier, Some(pinned_conversation::Identifier::GroupMasterKey(k)) if k == &vec![0xAB; 32]));
        assert_eq!(a.pinned_conversations[2].encode_to_vec(), raw_pin);
        assert_eq!((a.note_to_self_archived, a.note_to_self_marked_unread, a.read_receipts, a.username.as_str()), (false, true, true, "u"));
    }

    #[test]
    fn the_wire_walker_handles_every_scalar_wire_type_and_rejects_groups() {
        let mut m = vec![];
        wire::append_varint(&mut m, 1, 300);
        m.extend_from_slice(&[0x11, 1, 2, 3, 4, 5, 6, 7, 8]); // field 2, 64-bit
        wire::append_bytes(&mut m, 3, b"abc");
        m.extend_from_slice(&[0x25, 1, 2, 3, 4]); // field 4, 32-bit
        assert_eq!(wire::strip_fields(&m, &[2, 4]).unwrap(), { let mut v = vec![]; wire::append_varint(&mut v, 1, 300); wire::append_bytes(&mut v, 3, b"abc"); v });
        assert!(wire::strip_fields(&[0x0B], &[]).is_err(), "start-group");
        assert!(wire::strip_fields(&[0x0A, 0x05, 1], &[]).is_err(), "truncated length-delimited");
    }

    #[test]
    fn crypto_round_trip_through_libsignal_service() {
        use presage::libsignal_service::master_key::StorageServiceKey;
        use presage::libsignal_service::StorageService;
        let key = StorageServiceKey::from_slice(&[7u8; 32]).unwrap();
        let ikm = [3u8; 32];
        let raw_id = vec![5u8; 16];
        let item = StorageService::encrypt_item(&key, raw_id.clone(), &StorageRecord::decode(&*contact(ALICE)).unwrap(), Some(&ikm));
        let back = StorageService::decrypt_item(&key, &item, Some(&ikm)).unwrap();
        assert!(matches!(decode(&back.encode_to_vec()).unwrap(), Remote::Chat { .. }));
    }
```
(Check `StorageServiceKey::from_slice`'s exact signature in `master_key.rs:55` — it may take `&[u8]` and return `Result`; adjust the test, not the lib. `uuid` is a dependency already — `rg uuid core/sig-core/Cargo.toml`; if not, use `presage::libsignal_service::prelude::Uuid`.)

- [ ] **Step 2: Run, expect failures** (`cargo test -p sig-core --lib storage::records` → unresolved module).

- [ ] **Step 3: Implement** `storage/mod.rs` (types above, `pub(crate) mod records; pub(crate) mod wire` lives inside `records.rs`), the wire walker:

```rust
pub(crate) mod wire {
    /// (field, wire type, start, end) of each top-level field; `Err` on a group or a truncated message.
    fn fields(msg: &[u8]) -> Result<Vec<(u32, u8, usize, usize)>, String> {
        let mut out = vec![];
        let mut i = 0;
        while i < msg.len() {
            let start = i;
            let (tag, n) = varint(msg, i)?;
            i += n;
            let (field, wt) = ((tag >> 3) as u32, (tag & 7) as u8);
            match wt {
                0 => { let (_, n) = varint(msg, i)?; i += n; }
                1 => i += 8,
                2 => { let (len, n) = varint(msg, i)?; i += n + len as usize; }
                5 => i += 4,
                _ => return Err(format!("unsupported wire type {wt}")),
            }
            if i > msg.len() { return Err("truncated field".into()); }
            out.push((field, wt, start, i));
        }
        Ok(out)
    }
    fn varint(msg: &[u8], mut i: usize) -> Result<(u64, usize), String> { /* standard LEB128, ≤ 10 bytes, Err past the end */ }
    pub fn strip_fields(msg: &[u8], drop: &[u32]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(msg.len());
        for (field, _, s, e) in fields(msg)? { if !drop.contains(&field) { out.extend_from_slice(&msg[s..e]); } }
        Ok(out)
    }
    pub fn first_field(msg: &[u8]) -> Result<(u32, &[u8]), String> { /* exactly one field, wire type 2: return (field, payload) */ }
    pub fn append_varint(out: &mut Vec<u8>, field: u32, v: u64) { put_varint(out, (u64::from(field) << 3) | 0); put_varint(out, v); }
    pub fn append_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) { put_varint(out, (u64::from(field) << 3) | 2); put_varint(out, b.len() as u64); out.extend_from_slice(b); }
}
```
`decode` uses prost; `patch_chat`/`patch_account` use `first_field` + `strip_fields` + append, and re-wrap with `append_bytes(&mut out, outer_field, &inner)`.

- [ ] **Step 4: Run the tests to green**; `cargo clippy -p sig-core --all-targets -- -D warnings`.

- [ ] **Step 5: Commit** — `git commit -m "Storage Service records are decoded, and patched byte for byte for writing"`.

---

### Task 3: The HTTP client behind a trait

**Files:**
- Create: `core/sig-core/src/storage/client.rs`
- Modify: `core/sig-core/src/net.rs` (nothing new unless `push_service` needs `pub(crate)` elsewhere — it is already)

**Interfaces:**
- Produces:
```rust
pub(crate) enum StorageError { Conflict, Unauthorized, Invalid(String), Network(String) }
pub(crate) struct Manifest { pub version: u64, pub record_ikm: Vec<u8>, pub identifiers: Vec<(Vec<u8>, i32)> /* (key, kind) */ }
pub(crate) struct Write { pub manifest: Manifest, pub source_device: u32, pub inserts: Vec<(Vec<u8>, Vec<u8>)> /* (key, plaintext StorageRecord) */, pub deletes: Vec<Vec<u8>> }
pub(crate) trait StorageApi {
    async fn manifest_if_changed(&mut self, version: u64) -> Result<Option<Manifest>, StorageError>;   // version 0 → GET /v1/storage/manifest (no suffix)
    async fn read_items(&mut self, keys: &[Vec<u8>], record_ikm: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StorageError>; // (key, plaintext); batches of ≤ 2500
    async fn write(&mut self, w: &Write) -> Result<(), StorageError>;                                   // 409 → Conflict
}
pub(crate) struct StorageHttp { push: PushService, key: StorageServiceKey, auth: Option<HttpAuth> }
impl StorageHttp { pub(crate) fn new(data: &RegistrationData, key: StorageServiceKey) -> Result<Self, String> }
```
  - `auth()` (private): `GET /v1/storage/auth` with `HttpAuthOverride::NoOverride` → `{username,password}`; cached for the instance (one instance per run). 401/403 → `Unauthorized`.
  - Manifest: `GET Endpoint::storage("/v1/storage/manifest/version/{v}")` (`"/v1/storage/manifest"` for 0), `HttpAuthOverride::Identified(auth)`; 204 → `None`; 200 → protobuf `StorageManifest` → `StorageService::decrypt_manifest(&key, &m)` → `Manifest` (identifier `raw`, `r#type`); decrypt failure → `Invalid`.
  - Read: `PUT /v1/storage/read` with `ReadOperation { read_key }` chunked by 2 500, header `Content-Type: application/x-protobuf`; response `StorageItems`; each item `StorageService::decrypt_item(&key, &item, ikm_opt)` → `(item.key, record.encode_to_vec())`. **Note:** `decrypt_item` returns a prost `StorageRecord`, which loses unknown fields. To keep them, decrypt the raw bytes ourselves: copy `StorageService::item_key` logic? It is private. Instead use the public pair the way the lib allows: add a tiny `fn decrypt_raw(key, item, ikm) -> Result<Vec<u8>>` in `client.rs` that re-derives the item key exactly as `storage_service.rs` does (HKDF-SHA256 with info `b"20240801_SIGNAL_STORAGE_SERVICE_ITEM_" || raw_id` when `ikm` non-empty, else HMAC-SHA256(storage key, "Item_" + base64(raw_id))) and AES-256-GCM-decrypts `iv(12) || ct+tag`; same for encryption (`encrypt_raw`). Test both against the lib's `encrypt_item`/`decrypt_item` (round trips in both directions) so a lib change shows up. Dependencies needed: `aes-gcm`, `hkdf`, `hmac`, `sha2`, `base64` — all already in the tree via libsignal-service; add them to `sig-core/Cargo.toml` at the versions `cargo tree` shows.
  - Write: build `ManifestRecord { version, source_device, identifiers, record_ikm }` → `StorageService::encrypt_manifest`; items via `encrypt_raw`; `WriteOperation { manifest: Some(..), insert_item, delete_key, clear_all: false }` → `PUT Endpoint::storage("/v1/storage")`; 200/204 → `Ok`; 409 → `Conflict`; 401/403 → `Unauthorized`; other → `Network("storage write: status N")`.
  - `StorageServiceKey` from presage's master key: `StorageServiceKey::from_master_key(&master_key)` (done by the actor, Task 6).

- [ ] **Step 1: Write the failing tests** (`client.rs` tests): `raw_item_crypto_matches_the_library` (encrypt with ours → `decrypt_item` lib; encrypt with lib → `decrypt_raw` ours; both IKM and legacy paths; a wrong key fails), `manifest_paths_follow_the_version` (a pure fn `manifest_path(version: u64) -> String`), `read_keys_are_chunked` (pure `chunks(keys) -> Vec<&[Vec<u8>]>` of ≤ 2500), `a_write_operation_is_encoded_as_signal_expects` (pure `encode_write(&key, &Write) -> WriteOperation`: `clear_all == false`, one insert per record decryptable with the lib, manifest decrypts to the given identifiers and version).
- [ ] **Step 2: Run, expect failures.**
- [ ] **Step 3: Implement** as above (HTTP methods thin: status match on `resp.status().as_u16()`, bodies via `.bytes()` + prost decode; `reqwest::Error` → `Network(e.without_url().to_string())`).
- [ ] **Step 4: Tests green; clippy clean.**
- [ ] **Step 5: Commit** — `"A Storage Service client reads the manifest and items and writes a manifest at the next version"`.

---

### Task 4: Pull

**Files:**
- Create: `core/sig-core/src/storage/sync.rs`
- Test support: a `FakeApi` in `sync.rs` tests: `manifest: Option<Manifest>` (None = 204), `items: HashMap<Vec<u8>, Vec<u8>>` (plaintext per key; a key missing = not returned), `fail_items: HashSet<Vec<u8>>` (→ `Invalid`), `writes: Vec<Write>`, `conflicts_left: u32` (each write while > 0 → `Conflict`, decrement, and bump `manifest.version` by 1 with a new random identifier added — "someone wrote"), `unauthorized: bool`.

**Interfaces:**
- Consumes Task 1 db methods, Task 2 `decode`, Task 3 `StorageApi`.
- Produces:
```rust
pub(crate) struct Keys { pub fingerprint: Vec<u8> /* SHA-256 of the storage key bytes */ }
pub(crate) struct PullOutcome { pub applied: usize, pub chats_changed: bool, pub version: u64 }
pub(crate) async fn pull(api: &mut impl StorageApi, db: &ProjectionDb, keys: &Keys, me: &str) -> Result<PullOutcome, StorageError>
```
  Algorithm (spec "Pull"): (1) fingerprint mismatch → `reset_storage_state`; (2) `manifest_if_changed(stored.version)` → `None` ⇒ `Ok(PullOutcome{applied:0, chats_changed:false, version: stored})`; (3) `new = manifest.identifiers − stored keys`; `gone = stored − manifest`; (4) `read_items(new, ikm)`; keys not returned → dropped from the set (logged count); (5) decode each; `Invalid` from the api or `decode` error → return `Err(Invalid)` **before** any db write; (6) one pass: for every `Remote::Chat` → `apply_remote_chat`; collect the account record (there must be ≤ 1; two → take the last, log); after chats, `apply_remote_pins(mappable)`, note-to-self flags → `apply_remote_chat("c:<me>", flags-with-existing-mute-blocked-whitelisted)` (read the current row first; only archived/marked_unread change); (7) `replace_storage_items` with the manifest's full identifier list (mapping from decoded records, `None` for the rest; rows for keys we already had keep their old mapping), `set_storage_state { version, record_ikm, fingerprint, account_dirty: unchanged, unmappable_pins }`. Returns `chats_changed` (any `apply_*` reported a change).

- [ ] **Step 1: Write the failing tests** (`sync.rs` tests; each builds a db with `ProjectionDb::open("sqlite::memory:", None)` and a `FakeApi`):
  - `first_pull_applies_flags_pins_and_records_the_manifest`: manifest v3 with a contact (alice archived+muted), a group (marked unread, whitelisted), an account (pins: group, e164-raw, alice; noteToSelfMarkedUnread), a chat-folder item (Other). Assert alice/group rows and flags, pins (group 1, alice 2 — alice's remote archive is overridden by the pin: `archived == false`), `c:<me>` marked unread, `storage_items` has 4 rows with mappings (folder → None), state version 3 and `unmappable_pins == [e164 bytes]`, outcome `applied == 4`, `chats_changed`.
  - `an_unchanged_manifest_changes_nothing`: state v3, api manifest None → outcome version 3, no db change, no reads (`FakeApi.reads == 0`).
  - `only_new_keys_are_read_and_gone_keys_are_dropped`: stored items {A→alice, B→acct}; manifest v4 {B, C(new contact bob)} → reads == [C]; items after: {B, C}; alice's flags untouched.
  - `a_rewritten_record_overrides_a_dirty_local_change` (Review Focus 1): alice dirty (set_mute_until 9), manifest brings new key for alice with mute 0 → mute 0, not dirty.
  - `a_key_the_server_does_not_return_is_dropped_not_fatal` (Review Focus 3): manifest lists D, api has no item D → pull Ok, items exclude D.
  - `an_undecryptable_item_aborts_before_anything_is_written`: `fail_items = {C}` → `Err(Invalid)`, state still v3, alice unchanged.
  - `a_changed_storage_key_wipes_the_state_first` (Review Focus 5): state with fingerprint X, items; pull with fingerprint Y → api called with version 0; state rebuilt.
  - `unauthorized_is_surfaced`: `unauthorized = true` → `Err(Unauthorized)`.
  - `blocked_and_whitelisted_land_in_the_projection`: contact blocked → `conv.blocked`.
- [ ] **Step 2: Run, expect failures.** **Step 3: Implement.** **Step 4: Green + clippy.**
- [ ] **Step 5: Commit** — `"Chat settings are pulled from the Storage Service: the phone's flags and pins win"`.

---

### Task 5: Push and the run loop

**Files:**
- Modify: `core/sig-core/src/storage/sync.rs`

**Interfaces:**
- Produces:
```rust
pub(crate) struct PushOutcome { pub written: usize, pub skipped_no_record: usize }
pub(crate) async fn push(api: &mut impl StorageApi, db: &ProjectionDb, keys: &Keys, me: &str, device_id: u32) -> Result<PushOutcome, StorageError>
/// pull, then (writes on and something dirty) push; on Conflict: pull and retry after 1 s, 5 s, 30 s (cfg(test): 0 ms); gives up after the third conflict with `Err(Conflict)`.
pub(crate) async fn run(api: &mut impl StorageApi, db: &ProjectionDb, keys: &Keys, me: &str, device_id: u32, sleep: &dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()>>>) -> Result<RunOutcome, StorageError>
pub(crate) struct RunOutcome { pub pulled: PullOutcome, pub pushed: Option<PushOutcome> }
```
  `push`: `dirty = db.dirty_chats()`, `state = storage_state()` (None → nothing to do: never pushed before pulling); for each dirty chat with a `storage_items` row → `read_items([key])` → `patch_chat` → new key `rand::random::<[u8;16]>()`; no row → counted in `skipped_no_record`, stays dirty. `account_dirty` → the ACCOUNT row's item → `patch_account(raw, local pins in `pinned_order` order as `Pin::Chat`, then `unmappable_pins` as `Pin::Raw`, note-to-self flags from `c:<me>`'s row)`. Manifest: `version + 1`, identifiers = stored − replaced + new (same kinds). Invariants (`debug_assert!` + runtime `Err(Invalid)`): exactly one ACCOUNT, unique keys, every replaced key in deletes. Nothing to write → `Ok(written 0)` without a request. On `Ok`: `replace_storage_items`, `set_storage_state(version+1, account_dirty false)`, `clear_dirty(written chats)`. Writes off (`!db.storage_writes()`) → `push` returns `written 0` without reading.

- [ ] **Step 1: Write the failing tests:**
  - `push_rewrites_only_dirty_records_under_new_keys`: state v3 with items {A→alice, B→bob, C→acct}; alice dirty → one write: version 4, inserts 1 (new key ≠ A, plaintext decodes to alice's flags, unknown-field-preserving path exercised by a record with field 900), deletes [A], identifiers = {B, C, new}; after: items {B, C, new→alice}, alice not dirty, state v4.
  - `the_account_is_rewritten_with_local_pins_and_kept_raw_entries` (Review Focus 2): account_dirty, local pins bob(1), alice(2), `unmappable_pins = [raw]`; write's account plaintext → pins [bob, alice, raw]; `account_dirty` false after.
  - `a_dirty_chat_without_a_record_stays_dirty_and_is_counted`: carol dirty, no item → no request if nothing else; `skipped_no_record == 1`, still dirty.
  - `writes_off_means_no_write`: `set_storage_writes(false)` → `push` makes no request, dirty stays.
  - `a_conflict_pulls_and_retries_at_the_new_version` (Review Focus 4): `conflicts_left = 1`; `run` → writes.len() == 2, second at `first.version + 1`... precisely: the fake bumps its manifest to v4 on the conflict (adding a new remote item), the retry pulls (reads the new item) and writes v5 with the new item in its identifiers.
  - `three_conflicts_give_up_and_keep_the_marks`: `conflicts_left = 3` → `run` returns `Err(Conflict)`, 3 write attempts, alice still dirty, state version = the fake's current (pulled) version.
  - `run_pulls_then_pushes_and_announces_both`: a plain run with one dirty chat → `RunOutcome { pulled.applied, pushed: Some(written 1) }`.
- [ ] **Step 2: Run, expect failures. Step 3: Implement. Step 4: Green + clippy.**
- [ ] **Step 5: Commit** — `"Local chat settings are written to the Storage Service, with conflicts pulled and retried"`.

---

### Task 6: Actor wiring: triggers, the single-flight task, the key, and the sync message

**Files:**
- Modify: `core/sig-core/src/actor.rs` (`Queues.storage`, `Command::StorageSync`, trigger on `Ready`, on `FetchLatest`, on `Keys`; the task), `core/sig-core/src/storage/mod.rs` (`is_storage_fetch_latest`, `Trigger`), `core/sig-core/src/send.rs` (`Job::StorageChanged`), `core/sig-core/src/client.rs` (set_* send `Command::StorageSync`; `storage_writes`/`set_storage_writes`)
- Tests: `storage/mod.rs` (pure fns), `client.rs` (new calls), `send.rs` (the sync message builder)

**Interfaces:**
- `pub(crate) enum Trigger { Connected, Phone, Local, SettingsOn }` and `pub(crate) fn is_storage_fetch_latest(body: &ContentBody) -> bool` (SynchronizeMessage with `content: Some(Content::FetchLatest(f))` and `f.r#type == Some(fetch_latest::Type::StorageManifest as i32)`), `pub(crate) fn is_keys_sync(body: &ContentBody) -> bool`.
- `Command::StorageSync` (from the client after a flag change, and when the settings switch turns on).
- `Queues.storage: mpsc::UnboundedSender<Trigger>`; the receive loop sends `Connected` right after `CoreEvent::Ready` (actor.rs:1181), `Phone` when `is_storage_fetch_latest`, `Connected` when `is_keys_sync` (a new key: the fingerprint handles it).
- The task (`fn spawn_storage_sync(...)` next to `spawn_call_router`): owns `rx: UnboundedReceiver<Trigger>`; loop: wait for a trigger, then drain the queue, debounce 500 ms (cfg(test): none) for `Local`, then: `let master = m.store().fetch_master_key().await` → `None` → `tracing::info!("storage sync skipped: no master key yet")` once per run; else `StorageServiceKey::from_master_key(&master)` → `StorageHttp::new(registration, key)` → `run(...)`. Outcome → `l.on_event(CoreEvent::ConversationsChanged)` if `pulled.chats_changed`; after a push with `written > 0` → `queues.sends.send(Job::StorageChanged)`; errors → `CoreEvent::Error { message: "Syncing chat settings with the phone failed: {e}" }` (Unauthorized: "the server refused the storage credentials"; Conflict after retries: "another device kept writing; will retry on the next change"). Runs only while a manager exists (linked); the task is spawned at core start with the other routers and survives relinks like the call router (same `registration` Rc).
- `Job::StorageChanged` in `send.rs`: `m.send_message(me, SyncMessage { content: Some(Content::FetchLatest(FetchLatest { r#type: Some(Type::StorageManifest as i32) })), ..Default::default() }, now)`; builder `pub fn storage_changed_sync() -> SyncMessage` tested.
- `Client`: `pub async fn storage_writes(&self) -> Result<bool, CoreError>`, `pub async fn set_storage_writes(&self, on: bool) -> Result<(), CoreError>` (on → `Command::StorageSync`); `set_archived/set_pinned/set_mute_until/set_marked_unread` → after the db write, `let _ = self.send(Command::StorageSync)`; `mark_read` when it cleared the mark → the same.

- [ ] **Step 1: Failing tests:** `storage::tests::only_the_storage_manifest_fetch_latest_counts` (StorageManifest → true; LocalProfile → false; a Sent → false), `keys_sync_is_recognised`; `send::tests::the_storage_changed_sync_asks_for_the_manifest`; `client::tests::flag_changes_ask_for_a_storage_sync` (the test `Client` has a command channel? — `as_me` gives a client whose `send` goes to a receiver the test can drain: check `Client::open` in tests; if commands are dropped in the test harness, assert via a `storage_sync_requests: AtomicUsize` counter on the client under `cfg(test)`), `client::tests::storage_writes_switch_round_trips`.
- [ ] **Step 2: Run, expect failures. Step 3: Implement. Step 4: `cargo test -p sig-core --lib` green; clippy.**
- [ ] **Step 5: Commit** — `"The core syncs chat settings with the Storage Service on connect, on the phone's request, and after a local change"`.

---

### Task 7: App: the Settings switch, the new fields, bindings

**Files:**
- Modify: `app/Semaphore/SemaphoreApp.swift` (SettingsView: `Toggle("Sync chat settings to your phone", isOn: …)` under "Message history", only when `store.screen == .chats`), `app/Semaphore/AppStore.swift` (`private(set) var storageWrites = true` loaded at boot via `client.storageWrites()`; `func setStorageWrites(_ on: Bool)` → client, error → `lastError`), `app/SemaphoreTests/FakeClient.swift` (`conversation()` fixture: `blocked: false, whitelisted: false`; `storageWrites` answer + recorder), `app/SemaphoreTests/AppStoreTests.swift` (one test)
- Run `make core` first (new `Conversation` fields, two new client calls), then `make project` (no new files, harmless).

- [ ] **Step 1: Failing test** `testTheStorageWritesSwitchReadsAndWritesTheCore`: boot with `fake.storageWritesAnswer = false` → `store.storageWrites == false`; `store.setStorageWrites(true)` → `fake.storageWritesSet == [true]`.
- [ ] **Step 2: Run (unsigned xcodebuild test), expect compile failure. Step 3: Implement. Step 4: Suite green.**
- [ ] **Step 5: Commit** — `"Settings has a switch for writing chat settings to the phone's account"`.

---

### Task 8: Verify end to end, open the PR

- [ ] `make format && make lint`; `cargo test`; unsigned Swift suite; build the app ad-hoc and run it: with the phone linked, pin a chat on the phone → the Mac's list updates within seconds (the Core Log shows "storage sync: read N, applied M"); mute on the Mac → the phone shows the bell. Report what could and couldn't be checked (no certificate: the app runs ad-hoc signed; the phone test is the owner's).
- [ ] PR: title "Chat settings follow the phone through Signal's Storage Service", body with what/why/tests, `Part of #20`; `make card issue=20 status="In review"`; review pass (`/pr-review-toolkit:review-pr`), fixes, one PR comment.
