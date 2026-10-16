# Chat settings follow the phone: Storage Service sync (M4 design)

## Context
PR #49 gave chats four settings — archived, pinned, muted, marked unread — kept locally and
seeded once from the phone's backup at link time. Signal keeps these in the **Storage Service**:
encrypted, server-stored account state every device reads and writes (contacts, groups, account
settings). The phone writes there when the user pins or mutes a chat and tells the other devices
with a `FetchLatest(STORAGE_MANIFEST)` sync message; Signal Desktop does the same in the other
direction. Without it, a pin on the phone never reaches the Mac and a mute on the Mac never
reaches the phone.

What exists (verified 2026-10-16 on the pinned revisions):
- libsignal-service (fb92b13) has a read client — `StorageService::new` (auth), `manifest`,
  `manifest_if_changed(version)`, `read_items(keys, record_ikm)` — and the crypto for both
  directions (`encrypt_manifest`, `encrypt_item`). There is no write call; a write is one
  `PUT /v1/storage` with a `WriteOperation { manifest, insertItem, deleteKey, clearAll }`, which
  `PushService::request` + `Endpoint::storage` can make. Every blob is AES-256-GCM; manifest
  keys derive from the storage key and the version, item keys from the manifest's `recordIkm`
  (or, on old accounts, from the storage key).
- presage (971b14b) stores the master key at link (from the account entropy pool) and on a
  `Keys` sync message; `StorageServiceKey::from_master_key` derives the storage key. presage
  itself never touches the Storage Service, and passes `FetchLatest` sync messages through to
  us like any other content.
- Records: `ContactRecord` (aci/e164/pni, profile key, names, `blocked`=9, `whitelisted`=10,
  `archived`=11, `markedUnread`=12, `mutedUntilTimestamp`=13, …), `GroupV2Record`
  (`masterKey`=1, `blocked`=2, `whitelisted`=3, `archived`=4, `markedUnread`=5,
  `mutedUntilTimestamp`=6, …), `AccountRecord` (`pinnedConversations`=14 by service id, e164 or
  group master key; note-to-self archived/marked-unread; many account settings). The manifest
  lists every record's 16-byte key with its type; types we don't know exist (chat folders,
  notification profiles, call links, story lists).
- Signal Desktop's rules, read from its source (`ts/services/storage.preload.ts`,
  `storageRecordOps.preload.ts`): sync at startup and on `FetchLatest`; **sync before every
  write**; a changed record gets a **new random key** and the old key goes in `deleteKey`;
  **unknown record types and unknown fields are kept** and re-listed verbatim; the manifest
  version is `current + 1`, a 409 means someone else wrote first (three attempts, 1 s then 5 s
  apart, then give up until the next trigger); a linked device never `clearAll`s; remote values of the chat
  flags **overwrite local ones** outright; the remote pinned list replaces the local one; a
  record that can't be decrypted aborts the sync; nothing runs without the storage key.

## Decisions (owner, 2026-10-16)
- First sync carries the four chat flags **both ways**, and **reads** `blocked` and
  `whitelisted` into the projection (no UI yet; the blocking and message-request slices build
  on it).
- Writing to the account's shared storage is **on by default**, with a Settings switch to turn
  writes off. Reads are always on.

## Goals
1. A pin, archive, mute or mark-unread made on the phone shows on the Mac within seconds of the
   phone's sync message, and on every connect.
2. The same change made on the Mac shows on the phone (and other linked devices) the same way.
3. Nothing the Mac writes loses state the phone keeps: unknown record types, unknown fields,
   records we can't interpret, pins we can't map.
4. The Mac never writes when it can't be sure of the account's current state: it syncs first,
   writes at `version + 1`, and stops on conflicts rather than forcing.

## Non-goals (later slices or never)
- Creating storage records for chats that have none (see "Why no new records").
- Contact names, profile keys, usernames, avatars, nicknames, notes from `ContactRecord`.
- Account settings (link previews, read receipts…), chat folders, notification profiles, story
  settings, call links.
- Blocking/unblocking and accepting message requests from the Mac (read only here).
- Requesting the storage key from the phone when it is missing (logged; the key arrives at link
  time on current phones).

## Architecture
A new core module `core/sig-core/src/storage/`:
- `client.rs` — the HTTP side: auth (`GET /v1/storage/auth`), manifest-if-changed, read items,
  **write** (`PUT /v1/storage`, protobuf body, 409 surfaced as its own error). Behind a small
  trait `StorageApi` so the sync logic is tested against a fake.
- `records.rs` — pure functions over record bytes: decode a `StorageRecord` into what we keep
  (conversation id + flags + blocked/whitelisted, or the account's pinned list), and **patch**
  a record's raw plaintext for a write: drop every occurrence of the fields we own and append
  our values, leaving every other byte as it was. Patching the wire format (a 40-line tag/
  length walker) is what preserves fields our proto doesn't know; prost would drop them on a
  decode–encode round trip.
- `sync.rs` — the two operations, `pull` and `push`, over `StorageApi` + `ProjectionDb`.
- Actor glue: one `spawn_local` task per run, single-flight with a "wanted" flag (a trigger
  while a run is in flight runs once more after it), serialized so a pull and a push never
  interleave. The storage key comes from presage's store (`fetch_master_key`, actor thread
  only); the HTTP work runs on the local task set like the history fetch.

### Data (migration 0011)
- `storage_sync (id = 1, version INTEGER, record_ikm BLOB, key_fingerprint BLOB, account_dirty
  INTEGER, unmappable_pins BLOB)` — the manifest version we have applied, its record IKM,
  SHA-256 of the storage key it belongs to (a new key from a `Keys` sync message invalidates
  everything: wipe and pull from zero), whether a pin change is waiting to be written, and the
  pinned-list entries we couldn't map, kept as their raw bytes.
- `storage_items (key BLOB PRIMARY KEY, type INTEGER NOT NULL, conversation_id TEXT NULL)` —
  every identifier of the manifest we applied: its type and, for records we could map, the chat
  (`c:<aci>` or `g:<hex master key>`). Exactly one row has type ACCOUNT. Unmappable records
  (PNI-only contacts, e164-only pins, unknown types, undecodable ones) sit here with
  `conversation_id NULL` so the next manifest we write lists them unchanged.
- `conversations` gains `blocked INTEGER NOT NULL DEFAULT 0`, `whitelisted INTEGER NOT NULL
  DEFAULT 0`, and `storage_dirty INTEGER NOT NULL DEFAULT 0` (a local flag change not yet
  written). `storage_sync` also carries `account_dirty` (a pin change not yet written).
- Setting `storage_writes` (settings table; absent = on).
- `Conversation` (the app's record) gains `blocked` and `whitelisted`.

### Pull (read sync)
1. No storage key (presage has no master key) → log once per run, done. Fingerprint differs
   from the stored one → delete `storage_sync` and `storage_items` first.
2. Auth, then `manifest_if_changed(stored version)`. 204 → done.
3. Diff the manifest's identifiers against `storage_items`: *new* keys (not stored) and *gone*
   rows (stored, not in the manifest). Gone rows are deleted; the chat's local flags stay as
   they are (the phone removed the record — it will make a new one when it cares again).
4. Read the new keys in batches of at most 2 500 (`read_items`). A key the server doesn't
   return is dropped from our set. An item that doesn't decrypt or decode aborts the pull with
   an error, version unchanged (Desktop does the same and asks for keys; we log).
5. Apply, record by record, in manifest order with the account record last, and store the new
   version only at the end: a pull cut short leaves the old version, and the next one re-reads
   and re-applies the same records (applying is idempotent):
   - `ContactRecord` with an ACI (string or binary) → `c:<aci>`; `GroupV2Record` → `g:<hex>`.
     The chat row is created if it doesn't exist (empty title, no messages: not listed until
     one arrives, but its flags are in place when it does). Set `archived`, `marked_unread`,
     `mute_until` (0 = off; ≥ 2^63−1 = always), `blocked`, `whitelisted` from the record —
     remote wins, and `storage_dirty` is cleared for that chat (a local change the phone
     overtook is lost, as on Desktop). The account record is applied last, so a remote pin
     wins over a remote archive of the same chat (pinning unarchives, as in #49).
   - `AccountRecord` → the pinned list replaces ours: `pinned_order` 1..n for the mappable
     entries in remote order, NULL for every other chat; entries we can't map (e164-only
     contacts) are remembered as opaque bytes in `storage_sync.unmappable_pins` so a later
     write carries them. Note-to-self `archived`/`markedUnread` apply to `c:<own aci>`.
   - PNI-only contacts, GroupV1, unknown types: stored with `conversation_id NULL`.
6. Store the manifest's version and `recordIkm`, replace `storage_items`, announce
   `ConversationsChanged` if any chat changed.

### Push (write)
Runs after a local change to any of the four flags (debounced 500 ms, coalesced), only when
`storage_writes` is on, and always **after a pull** in the same run (so our view is current
and the version is the server's).
1. Dirty chats with a mapped record: `read_items` their current bytes (fresh, so unknown fields
   are current), patch the owned fields (`archived`, `markedUnread`, `mutedUntilTimestamp`),
   encrypt under a **new random 16-byte key**; the old key goes to `deleteKey`.
2. `account_dirty`: patch the account record's `pinnedConversations` to our local pinned list
   in order, followed by the remembered unmappable entries; a flag change on Note to Self patches
   `noteToSelfArchived`/`noteToSelfMarkedUnread` (its mute has no field: local only).
3. Manifest: `version = stored + 1`, `sourceDevice = our device id`, identifiers = stored set
   minus deleted keys plus the new ones, `recordIkm` unchanged (never added, never removed).
   Invariants checked before sending: exactly one ACCOUNT identifier, no duplicate keys, every
   replaced key in `deleteKey`, `clearAll = false` always.
4. `PUT /v1/storage`. 2xx → replace the rows for the rewritten records, store the version, clear
   the dirty marks, send a `FetchLatest(STORAGE_MANIFEST)` sync message so the phone and other
   devices pull (as Desktop does). Only marks still matching what was written are cleared: a
   flag or pin changed while the write was in flight goes out with the next push. 409 → someone
   wrote first: pull, then retry after 1 s, then 5 s; after the third conflict leave the dirty
   marks and stop until the next trigger.
   Other errors → leave the dirty marks, log, retry at the next trigger.
5. A dirty chat **without** a record stays dirty and local; every later pull checks whether the
   phone has created one since, and the next push then writes it.

### Why no new records
A fresh `ContactRecord`/`GroupV2Record` must carry `whitelisted` (profile sharing / message
request accepted) and `blocked`, and phones take those from the record. A record we invent with
`whitelisted = false` turns a chat into a message request on the phone; with `true` it accepts
one the user didn't. We don't know the phone's state for a chat it never synced, so we don't
guess. Phones sync a record for every chat they have, so in practice every chat with messages
has one.

### Triggers
- `Ready` (connected and caught up): pull, then push if anything is dirty.
- `SyncMessage::FetchLatest { type: STORAGE_MANIFEST }` from our account: pull (+ push).
- A `Keys` sync message (presage stored a master key): pull; the fingerprint check handles a
  changed key.
- `Client::set_archived/set_pinned/set_mute_until/set_marked_unread`: mark dirty in the same
  transaction as the change, then `Command::StorageSync`.
- Settings switch turned on: push.

### App
- Settings: "Sync chat settings to your phone" (on by default) under the existing settings.
- `Conversation.blocked`/`whitelisted` exposed; no UI change in this slice.
- Core Log lines on each run: counts (records read, applied, written) and errors — never record
  contents, names or keys.

### Failure handling
| Situation | Behaviour |
|---|---|
| No master key in presage's store | log once per run, skip; nothing dirty is lost |
| 401/403 on auth or any call | log, stop until the next connect |
| Network error | log, retry at the next trigger |
| Undecryptable/undecodable item | abort the pull, keep the old version, log the key's type |
| 409 on write | pull, retry with backoff, three strikes then wait for the next trigger |
| Record of unknown type | listed unchanged in every manifest we write |
| PNI-only or e164-only contact | ignored for flags, preserved in the manifest and the pinned list |
| Pinned group we don't know | the chat row is created (we have the master key) and pinned |
| Storage key changed | state wiped, full pull |
| A panic in the task | reported like the history task's: logged, state untouched; syncing resumes at the next start |

Every failure but a missing master key is also reported to the app (Core Log and error banner).

### Testing
- `records.rs`: decode of contact/group/account fixtures (string and binary ACIs, "always"
  mute, pins of each kind); the patcher round-trips a record that carries a field our proto
  doesn't know and a repeated field, and the result decodes with the new values and the unknown
  bytes intact; manifest and item crypto round-trip through libsignal-service's helpers.
- `sync.rs` against a fake `StorageApi` and an in-memory projection: first pull creates rows
  and applies flags; 204 changes nothing; gone keys drop; pins replace, unmappable pins are kept
  and re-written; note-to-self flags; undecryptable item aborts without touching the version;
  push rewrites only dirty records with new keys and deletes old ones, manifest at version + 1
  with every untouched identifier; a 409 pulls and retries, the third conflict stops; a dirty
  chat with no record stays dirty; writes off → push does nothing; key fingerprint change wipes
  and pulls; blocked/whitelisted land in the projection.
- Actor: `FetchLatest` reaches the sync, a local change marks dirty and triggers it.
- App: the Settings switch calls the core; `Conversation` fixtures carry the new fields.
- Real account: the owner's check list in the PR (pin on the phone → Mac; mute on the Mac →
  phone; verify with the phone's chat list that nothing else changed).

### Open questions for the owner
None blocking. One note: the first pull on an account whose phone has pinned chats the Mac
never saw will create chat rows for them (invisible until a message), which is harmless.
