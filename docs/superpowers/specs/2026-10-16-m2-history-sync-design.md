# Message history on link (Milestone 2 design)

## Context
Signal Desktop can bring your past messages over when it is linked ("Transfer message history",
called link-and-sync). Semaphore, linked through presage, starts empty. M2 (#18) closes that gap.

How it works on the wire (researched from Signal-Desktop f3040f3, Signal-Server, Signal-Android
and libsignal v0.99.0):
- **The ask.** A linking device that adds `capabilities=backup5` to its provisioning URL (the QR
  code) is offered "Transfer message history" on the phone. If the user accepts, the provision
  message carries a 32-byte `ephemeralBackupKey`. presage exposes it as
  `Manager::ephemeral_backup_key()`. Today libsignal-service-rs builds the URL without
  `capabilities`, so the phone never offers it.
- **The archive.** The phone uploads an encrypted archive. The new device long-polls
  `GET /v1/devices/transfer_archive?timeout=…` (authenticated as itself), which answers:
  - `200 {cdn, key}`: the archive is ready
  - `200 {error: RELINK_REQUESTED | CONTINUE_WITHOUT_UPLOAD}`
  - `204`: not yet

  The device then downloads `{cdn}/attachments/{key}` from the transit CDN with a plain,
  unauthenticated GET (libsignal-service-rs `get_attachment` already does this).
- **The format.** The file is `IV(16) ‖ AES-256-CBC(gzip(varint-delimited frames)) ‖
  HMAC-SHA256(32)`.
  - Keys: `BackupKey(ephemeral)` → `backup_id(our ACI)` → `MessageBackupKey` (HKDF). There is no
    forward-secrecy token.
  - Frames: `BackupInfo` first, then account, recipient, chat, chatItem and others
    (libsignal `rust/message-backup/src/proto/backup.proto`).
- **Files.** The archive holds pointers, not file bodies. Transit-CDN files expire about 45 days
  after upload.

Neither presage nor libsignal-service-rs implements the transfer-archive endpoint or the import
(presage issue #396). Everything below lives in `sig-core`. presage is not forked or bumped.

## Decisions (owner, 2026-10-16)
See `docs/decisions/2026-10-16-history-import-merges.md`.
1. **Import straight into projection.db**, like Desktop's importer. Only what presage needs to send
   goes into presage's store.
2. **Relinking keeps existing data and merges history in.** This is beyond Desktop, which only
   asks on a fresh install. The fallback, if merging misbehaves, is Desktop's behaviour: wipe on
   relink.
3. **Media: recent only, like Desktop.** Attachment backfill (asking the phone to upload a file
   again) is out of scope.

## Scope
**In:**
- ask for history on first link and on "Relink to transfer history"
- wait for, download, verify, decrypt, parse and import the archive
- progress screen with cancel and retry
- resume after a quit or crash

**Out:**
- attachment backfill
- importing call history, stories, sticker packs, chat folders, notification profiles, account
  settings or usernames
- importing payment details and poll votes (they become M1's placeholders)

## Components
All new core code is in `core/sig-core/src/history/`.

- **QR code (`actor.rs` link path).** When history is wanted, append `capabilities=backup5` to the
  URL presage sends over its provisioning channel, before it reaches the app.
- **Fetcher (`history/fetch.rs`).**
  - Long-polls `transfer_archive` in 5-minute requests within a 1-hour budget, through
    `PushService::request` with the device's credentials.
  - Downloads the archive to `Application Support/Semaphore/history.tmp`, resuming from the bytes
    it already has.
  - HTTP sits behind a small trait (`ArchiveServer`) so tests can play the server's answers.
- **Reader (`history/read.rs`).**
  - Uses `libsignal-message-backup` (same libsignal tag as our lock) for `MessageBackupKey` and
    `FramesReader` (HMAC check, AES-CBC, gzip) and for `VarintDelimitedReader`.
  - Decodes frames with our own prost types generated from a copy of `backup.proto`, pinned to
    that tag (libsignal's own `proto` module is crate-private).
  - Runs on a blocking thread and streams frames over a bounded channel.
  - The whole file's HMAC is verified before the first frame is handed on.
- **Importer (`history/import.rs`).** It maps frames to rows in batches of about 1,000 items per
  transaction:
  - **Recipients and chats → conversations.**
    - A contact becomes `c:<ACI>`, or `c:PNI:<PNI>` if it has no ACI. Its title and profile key
      come along.
    - A group becomes `g:<hex master key>`, with its title.
    - The chat's disappearing timer comes along.
    - A recipient with only a phone number can't be addressed: it is skipped and counted.
  - **Chat items → messages.**
    - Author, sent_at, received_at, body, quote, edits (as revisions), deleted state, read and
      send status, `expires_in`, and `expires_at` (the real expiry).
    - Items whose expiry has already passed are skipped.
    - Contact, sticker, payment, poll and gift-badge items become M1's `MessageKind`
      placeholders.
    - Group, profile and timer updates become chat events where M1 already has a kind for them
      (timer changes). Others are skipped and counted.
  - **Reactions → reactions.**
  - **Attachments → attachment rows** with their transit pointer. One counts as downloadable only
    if it has a `transitCdnKey` and was uploaded within the last 90 days (Desktop's rule, 2× the
    45-day server queue time). Otherwise it is marked expired ("no longer available").
  - **Contacts, profile keys and group master keys** are collected and written to presage's store
    on the core thread when the import ends.
- **State (`history_import` table, projection.db).** It holds:
  - the state (`waiting`, `downloading` or `importing`)
  - the ephemeral key and our ACI
  - `cdn`, `cdn_key`, `bytes_done` and `total_bytes`

  projection.db is SQLCipher-encrypted, so the key is encrypted at rest. The row is deleted
  when the import finishes or is abandoned.
- **Relink (`client.rs`).** `relink_for_history()` stops receiving, closes and deletes presage.db,
  keeps projection.db, and starts linking with history wanted.
- **App.**
  - Settings gets "Relink to transfer history…", with a confirmation that explains the Mac becomes
    a new linked device.
  - A progress screen shows Desktop's steps: "Preparing…", "Downloading X of Y (n%)",
    "Importing…". It offers Cancel, and Retry or Continue without history on failure.

## Data flow
1. **Ask.** First link, or Relink, shows the QR code with `capabilities=backup5`.
2. **Link.** presage links. With no ephemeral key (declined on the phone), finish as in M1.
3. **Save.** Store the `history_import` row (`waiting`). **Receiving does not start** until step 7.
   New messages wait in the server queue and arrive after the history, in order.
4. **Wait and download.** Long-poll, then download with resume (`downloading`). The core emits
   `HistoryProgress { stage, done, total }`.
5. **Verify and read.** Verify the HMAC over the whole file, then stream frames (`importing`).
6. **Import.** Batched transactions. The merge rules:
   - a message already present (same author and sent_at) is kept, not overwritten
   - read status takes the higher of the two
   - reactions: the latest per reactor wins
   - edits: revisions are inserted if missing
7. **Finish.**
   - Write the collected contacts and groups to presage's store.
   - Delete the temp file and the state row.
   - Emit `ConversationsChanged` and start receiving.
   - Live messages that were also in the archive are dropped by the same (author, sent_at) key.

## Error handling
| Case | Behaviour |
|---|---|
| Declined on the phone (no key) | Normal link |
| `CONTINUE_WITHOUT_UPLOAD`, or 404 on the archive | "Your phone didn't send message history"; continue linked |
| `RELINK_REQUESTED` | Same as above; if the phone also removed the device, M1's unlinked screen takes over |
| No archive within 1 hour | Keep waiting, or Continue without history |
| Download error | Retry (resumes), or Continue without history |
| HMAC, decryption or parse failure | Nothing imported; "Message history couldn't be read"; continue; error in the Core Log |
| Unknown frame or item kind | Skipped; per-kind counts logged (never content) |
| Quit or crash | On launch, resume from the state row: re-download if incomplete, then re-import (idempotent) |
| Cancel | Stop, delete the temp file and the state row, continue linked without history |

Where this differs from Desktop:
- Desktop unlinks on cancel and on fatal errors. We stay linked, since existing data is kept.
- Desktop leaves the device unlinked after a crash mid-import. We resume the import instead.

Privacy: as in M1, logs carry ids, counts and errors only. The temp archive stays encrypted on disk
and is deleted when done. Imported disappearing messages keep their real expiry, so M1's sweep
removes them on time.

## Testing
- **Reader.** A test helper writes archives in the real format with a known key. Tests check:
  - a round trip
  - one flipped byte fails the HMAC check and nothing is imported
  - a truncated file
  - an unknown frame kind is skipped

  If libsignal ships an encrypted test archive with a known key, read it too, to prove
  compatibility.
- **Fetcher.** A fake `ArchiveServer` plays these server answers:
  - `204` then `200`
  - each error answer
  - a timeout
  - a download cut mid-way that resumes at the right byte
  - state that survives a restart
- **Importer.** Synthetic archives go into in-memory projection.db. Tests cover:
  - each item kind's mapping
  - group chats
  - the 90-day attachment rule
  - placeholders
  - timers, with expired items skipped
  - merges (existing rows kept, read status max, reactions)
  - a second import changes nothing
  - phone-number-only recipients skipped
- **Volume.** A generated archive of 100,000 messages is imported; time and peak memory go in the
  PR.
- **App.** Progress-screen states through `FakeClient`, rendered and looked at.
- **On the owner's phone, before merging:**
  - relink with history
  - relink declining it
  - cancel on the phone
  - quit the Mac app mid-download and relaunch

## Open questions (checked during implementation)
- Whether the phone still sends the contacts and groups sync after link-and-sync. If not, the
  importer's contacts and groups are the only source.
- Which libsignal validation `Purpose` matches link-and-sync archives (Android uses transfer).
  Validation is optional; we parse leniently.
- Whether `libsignal-message-backup` unifies cleanly with our workspace's dependency versions. It
  builds standalone with no OpenSSL or BoringSSL. If it doesn't fit, fall back to about 100 lines
  of our own HMAC, AES and gzip code.

## Estimate
About 1.5–2.5 weeks:
- plumbing, decryption and reader: 1–2 days
- importer: 5–8 days
- relink and progress UI: 2–3 days
- testing on the phone
