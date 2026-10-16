# Message History on Link (M2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When Semaphore is linked (first link or an explicit relink), it downloads the phone's
link-and-sync archive and imports the message history into projection.db, with progress, cancel,
retry, and resume after a crash.

**Architecture:** The QR code asks for history (`capabilities=backup5`). After linking, presage
hands us a one-time 32-byte ephemeral backup key; we persist it in a `history_import` row in
projection.db (SQLCipher-encrypted). A task on the core thread long-polls
`GET /v1/devices/transfer_archive`, downloads the archive from the transit CDN with resume,
verifies/decrypts/un-gzips it with `libsignal-message-backup`, and imports frames in batched
transactions straight into projection.db. Contacts, profile keys and group master keys are also
written to presage's store so sending works. Receiving starts only after the import ends (the
server queues messages meanwhile, so nothing is lost).

**Tech Stack:** Rust (sig-core), `libsignal-message-backup` (git tag v0.99.0, feature
`test-util` — makes its `proto` and `export` modules public, so we need no proto copy),
`protobuf` 3.7.2 (frame decoding), `serde_json` (server answers and reading presage's
registration password), sqlx/SQLCipher, uniffi, SwiftUI.

**Spec:** `docs/superpowers/specs/2026-10-16-m2-history-sync-design.md`
(decisions: `docs/decisions/2026-10-16-history-import-merges.md`)

## Global Constraints

- presage stays pinned at rev `971b14bf1b7382e45bc363966e1ed588173e7c36`; no fork, no bump.
- All libsignal git deps use tag `v0.99.0` (checkout 439f035), same as libsignal-service's pins.
- presage's `Manager`/store are `!Send`: touch them only on the core thread (actor's LocalSet).
- Never log message content; ids, counts and errors only.
- Never edit `app/Generated/`; regenerate with `make core`. Xcode project via `make project`.
- No `--no-verify`; verify with `make format && make check` before each push.
- Work in `/Users/pitis/development/semaphore/worktrees/47--history-sync`, branch `feat/history-sync`.
- Commit messages: plain sentences, no `#NNN`, no AI attribution.
- `PushService`'s HTTP client has a 65 s read timeout → long-poll requests use `timeout=60`.

## Review Focus

1. **A server that ignores the Range header** (returns 200 with the whole file instead of 206
   from the offset) — the download must restart the file from byte 0, not append. Test in Task 4.
2. **Resume with a complete file on disk** (crash after download, before import) — the driver
   must go straight to importing, not re-request bytes past the end. Test in Task 4.
3. **A corrupt complete file** (HMAC fails on resume) — bytes_done must reset so Retry
   re-downloads instead of failing forever. Test in Task 7.
4. **A chat whose recipient was skipped** (phone-number-only contact) — its chat items must be
   skipped and counted, not crash or land in a broken conversation. Test in Task 5.
5. **The same message in the archive and in the queued live messages** — after import, receiving
   drains the server queue; `(author, sent_at)` INSERT OR IGNORE must deduplicate, and merged
   status must never downgrade. Tests in Task 6.

---

### Task 1: Dependencies and the QR capability

The phone offers "Transfer message history" only when the provisioning URL carries
`capabilities=backup5`. libsignal-service builds the URL without it; we append it in the actor
before showing the QR code. This task also adds every new Cargo dependency, which answers the
spec's open question (does `libsignal-message-backup` unify with our tree?) before anything is
built on it.

**Files:**
- Modify: `core/sig-core/Cargo.toml`
- Modify: `core/sig-core/src/actor.rs` (the `link` fn, ~line 431)
- Create: `core/sig-core/src/history/mod.rs` (just the module + capability helper for now)
- Modify: `core/sig-core/src/lib.rs` (add `mod history;`)

**Interfaces:**
- Produces: `history::with_backup_capability(url: &str) -> String`.

- [ ] **Step 1: Add dependencies**

In `core/sig-core/Cargo.toml` under `[dependencies]`:

```toml
libsignal-message-backup = { git = "https://github.com/signalapp/libsignal", tag = "v0.99.0", features = ["test-util"] }
protobuf = "3.7.2"
serde_json = "1"
```

`test-util` makes the crate's `proto` module (protobuf-generated backup.proto types) and
`export` module (gzip/AES/HMAC helpers for writing archives in tests) public. All its deps are
pure Rust — no OpenSSL/BoringSSL, so no SQLCipher conflict. `protobuf` 3.7.2 is already in
Cargo.lock (libsignal pins it).

- [ ] **Step 2: Verify the dependency unifies**

Run: `cd core && cargo check -p sig-core 2>&1 | tail -20`
Expected: compiles (warnings fine). If cargo picks a *second* libsignal checkout, the tags
diverged — stop and report; do not work around it with patches.

- [ ] **Step 3: Write the failing test**

Create `core/sig-core/src/history/mod.rs`:

```rust
//! Importing the phone's message history at link time (link-and-sync).

/// The provisioning URL with `capabilities=backup5` appended, which makes the phone offer
/// "Transfer message history" when it scans the QR code.
pub(crate) fn with_backup_capability(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&capabilities=backup5")
    } else {
        format!("{url}?capabilities=backup5")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_qr_url_asks_for_history() {
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice?uuid=u&pub_key=k"),
            "sgnl://linkdevice?uuid=u&pub_key=k&capabilities=backup5"
        );
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice"),
            "sgnl://linkdevice?capabilities=backup5"
        );
    }
}
```

Add `mod history;` to `core/sig-core/src/lib.rs` (after `mod events;`).

- [ ] **Step 4: Run the test** — `cd core && cargo test -p sig-core history` → PASS (the helper
  is written with its test; the behavioral gap it closes is wired in the next step).

- [ ] **Step 5: Use it in the link path**

In `actor.rs` `link()`, the URL event becomes:

```rust
if let Ok(url) = url_rx.await {
    l.on_event(CoreEvent::LinkUrl {
        url: crate::history::with_backup_capability(url.as_str()),
    });
}
```

(`url` is a `url::Url`; `as_str()` avoids a re-parse.)

- [ ] **Step 6: Verify** — `cd core && cargo test -p sig-core` → all pass.

- [ ] **Step 7: Commit** — `git add -A && git commit -m "The linking QR code asks the phone for message history"`

---

### Task 2: The history_import state row

One row in projection.db survives quits and crashes: which stage the import is in, the ephemeral
key, our ACI, and download progress. projection.db is SQLCipher-encrypted, so the key is
encrypted at rest.

**Files:**
- Create: `core/sig-core/migrations/0009_history_import.sql`
- Modify: `core/sig-core/src/projection/db.rs` (new methods at the end of `impl ProjectionDb`)
- Modify: `core/sig-core/src/projection/mod.rs` (re-export the new types)

**Interfaces:**
- Produces:
  - `pub struct HistoryImportState { pub stage: HistoryDbStage, pub key: Vec<u8> /* 32 bytes */, pub aci: String, pub cdn: Option<u32>, pub cdn_key: Option<String>, pub bytes_done: u64, pub total_bytes: Option<u64> }`
  - `pub enum HistoryDbStage { Waiting, Downloading, Importing }`
  - `ProjectionDb::history_state() -> Result<Option<HistoryImportState>, sqlx::Error>`
  - `ProjectionDb::set_history_state(&HistoryImportState) -> Result<(), sqlx::Error>` (upsert)
  - `ProjectionDb::clear_history_state() -> Result<(), sqlx::Error>`

- [ ] **Step 1: Migration**

`core/sig-core/migrations/0009_history_import.sql`:

```sql
-- One in-flight history import (link-and-sync): resumed on launch, deleted when done or
-- abandoned. `key` is the one-time ephemeral backup key from the provisioning message.
CREATE TABLE history_import (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  stage TEXT NOT NULL CHECK (stage IN ('waiting', 'downloading', 'importing')),
  key BLOB NOT NULL CHECK (length(key) = 32),
  aci TEXT NOT NULL,
  cdn INTEGER,
  cdn_key TEXT,
  bytes_done INTEGER NOT NULL DEFAULT 0,
  total_bytes INTEGER
);
```

- [ ] **Step 2: Write the failing test** (in `db.rs`'s `mod tests`)

```rust
#[tokio::test]
async fn history_state_round_trips_and_clears() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    assert_eq!(db.history_state().await.unwrap(), None);
    let mut s = HistoryImportState {
        stage: HistoryDbStage::Waiting,
        key: vec![7u8; 32],
        aci: "22222222-2222-2222-2222-222222222222".into(),
        cdn: None,
        cdn_key: None,
        bytes_done: 0,
        total_bytes: None,
    };
    db.set_history_state(&s).await.unwrap();
    assert_eq!(db.history_state().await.unwrap(), Some(s.clone()));
    s.stage = HistoryDbStage::Downloading;
    s.cdn = Some(3);
    s.cdn_key = Some("abc".into());
    s.bytes_done = 1024;
    s.total_bytes = Some(4096);
    db.set_history_state(&s).await.unwrap();
    assert_eq!(db.history_state().await.unwrap(), Some(s), "updates replace the row");
    db.clear_history_state().await.unwrap();
    assert_eq!(db.history_state().await.unwrap(), None);
}
```

- [ ] **Step 3: Run it** — `cargo test -p sig-core history_state` → FAIL (types don't exist).

- [ ] **Step 4: Implement**

In `db.rs` (types near `Expired`, methods at the end of `impl ProjectionDb`):

```rust
/// Where an interrupted history import picks up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryDbStage {
    Waiting,
    Downloading,
    Importing,
}

impl HistoryDbStage {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Downloading => "downloading",
            Self::Importing => "importing",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [Self::Waiting, Self::Downloading, Self::Importing]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

/// The one in-flight history import, surviving quits and crashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryImportState {
    pub stage: HistoryDbStage,
    /// The 32-byte ephemeral backup key from the provisioning message.
    pub key: Vec<u8>,
    /// Our ACI (part of the decryption key derivation).
    pub aci: String,
    pub cdn: Option<u32>,
    pub cdn_key: Option<String>,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
}
```

Methods:

```rust
pub async fn history_state(&self) -> Result<Option<HistoryImportState>, sqlx::Error> {
    let row = sqlx::query("SELECT stage, key, aci, cdn, cdn_key, bytes_done, total_bytes FROM history_import WHERE id = 1")
        .fetch_optional(&self.pool)
        .await?;
    row.map(|r| {
        let stage: String = r.try_get("stage")?;
        let cdn: Option<i64> = r.try_get("cdn")?;
        let bytes_done: i64 = r.try_get("bytes_done")?;
        let total: Option<i64> = r.try_get("total_bytes")?;
        Ok(HistoryImportState {
            stage: HistoryDbStage::parse(&stage)
                .ok_or_else(|| decode_err(format!("history stage {stage:?}")))?,
            key: r.try_get("key")?,
            aci: r.try_get("aci")?,
            cdn: cdn.map(|c| c as u32),
            cdn_key: r.try_get("cdn_key")?,
            bytes_done: bytes_done as u64,
            total_bytes: total.map(|t| t as u64),
        })
    })
    .transpose()
}

pub async fn set_history_state(&self, s: &HistoryImportState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO history_import (id, stage, key, aci, cdn, cdn_key, bytes_done, total_bytes)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET stage = ?1, key = ?2, aci = ?3, cdn = ?4, cdn_key = ?5,
           bytes_done = ?6, total_bytes = ?7",
    )
    .bind(s.stage.as_str())
    .bind(&s.key)
    .bind(&s.aci)
    .bind(s.cdn.map(|c| c as i64))
    .bind(&s.cdn_key)
    .bind(s.bytes_done as i64)
    .bind(s.total_bytes.map(|t| t as i64))
    .execute(&self.pool)
    .await?;
    Ok(())
}

pub async fn clear_history_state(&self) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM history_import").execute(&self.pool).await?;
    Ok(())
}
```

(`decode_err` already exists in db.rs.) Re-export in `projection/mod.rs`:
`pub use db::{Expired, HistoryDbStage, HistoryImportState, ProjectionDb};`

- [ ] **Step 5: Run it** — `cargo test -p sig-core history_state` → PASS; then full
  `cargo test -p sig-core` → all pass.

- [ ] **Step 6: Commit** — `git commit -am "A history import's stage and key survive restarts"`

---

### Task 3: Reading the archive

`history/read.rs` turns the downloaded file into a stream of decoded `Frame`s. The whole file's
HMAC is verified before the first frame is handed out (FramesReader does this). The CPU-heavy
part (AES-CBC, gzip) runs on a blocking thread; frames flow over a bounded channel to the core
thread.

A test helper writes real archives (IV ‖ AES-256-CBC(gzip(varint-delimited frames)) ‖ HMAC) with
`libsignal_message_backup::export` so reader tests prove the real format.

**Files:**
- Create: `core/sig-core/src/history/read.rs`
- Create: `core/sig-core/src/history/test_archive.rs` (`#[cfg(test)]`)
- Modify: `core/sig-core/src/history/mod.rs` (`mod read;` + `#[cfg(test)] mod test_archive;` + the `HistoryError` type)

**Interfaces:**
- Produces:
  - `pub(crate) enum HistoryError { Server(String), Download(String), Archive(String), Db(sqlx::Error) }` (thiserror, in mod.rs)
  - `read::stream(path: PathBuf, key: [u8; 32], aci: Uuid) -> tokio::sync::mpsc::Receiver<Result<Frame, String>>` where `Frame = libsignal_message_backup::proto::backup::Frame`. An `Err` item ends the stream (HMAC/decrypt/parse failure); channel close = end of file.
  - `test_archive::write(path: &Path, key: &[u8; 32], aci: Uuid, frames: &[Frame])`
  - `test_archive::message_backup_key(key: &[u8; 32], aci: Uuid) -> MessageBackupKey`

**Note on generated names:** the proto types come from protobuf-codegen; fields are snake_case
(`chatId` → `chat_id`), oneofs are enums in a submodule (`Frame.item` →
`Option<frame::Item>` with variants `Item::Account/Recipient/Chat/ChatItem/...`), message fields
are `protobuf::MessageField<T>`, proto enums are `protobuf::EnumOrUnknown<T>`. If a name doesn't
compile, read the generated file:
`find core/target/debug/build -path '*libsignal-message-backup*/out/protos/*' -name '*.rs'`.

- [ ] **Step 1: HistoryError in mod.rs**

```rust
/// Why a history import stopped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryError {
    /// The chat server refused or couldn't be reached.
    #[error("server: {0}")]
    Server(String),
    /// The archive download failed.
    #[error("download: {0}")]
    Download(String),
    /// The archive couldn't be verified or read (HMAC, decryption, gzip, protobuf).
    #[error("the archive couldn't be read: {0}")]
    Archive(String),
    #[error("storage: {0}")]
    Db(#[from] sqlx::Error),
}
```

- [ ] **Step 2: The test archive writer**

`core/sig-core/src/history/test_archive.rs`:

```rust
//! Writes archives in the real link-and-sync format, so tests prove the format, not a mock.

use libsignal_message_backup::export::{aes_cbc_encrypt, gzip_compress, hmac_checksum, pad_gzipped_bucketed};
use libsignal_message_backup::key::MessageBackupKey;
use libsignal_message_backup::proto::backup::{BackupInfo, Frame};
use presage::libsignal_service::libsignal_account_keys::BackupKey;
use presage::libsignal_service::protocol::Aci;
use presage::libsignal_service::prelude::Uuid;
use protobuf::Message as _;

pub fn message_backup_key(key: &[u8; 32], aci: Uuid) -> MessageBackupKey {
    let backup_key = BackupKey(*key);
    let id = backup_key.derive_backup_id(&Aci::from(aci));
    MessageBackupKey::derive(&backup_key, &id, None)
}

/// IV ‖ AES-256-CBC(gzip(varint-delimited BackupInfo + frames)) ‖ HMAC-SHA256, like the phone.
pub fn write(path: &std::path::Path, key: &[u8; 32], aci: Uuid, frames: &[Frame]) {
    let mbk = message_backup_key(key, aci);
    let mut plain = BackupInfo {
        version: 1,
        backup_time_ms: 1_700_000_000_000,
        ..Default::default()
    }
    .write_length_delimited_to_bytes()
    .unwrap();
    for f in frames {
        plain.extend(f.write_length_delimited_to_bytes().unwrap());
    }
    let mut body = gzip_compress(futures::io::Cursor::new(plain));
    pad_gzipped_bucketed(&mut body);
    let iv = [0x24u8; 16];
    aes_cbc_encrypt(&mbk.aes_key, &iv, &mut body);
    let hmac = hmac_checksum(&mbk.hmac_key, &iv, &body);
    let mut file = Vec::with_capacity(16 + body.len() + 32);
    file.extend(iv);
    file.extend(body);
    file.extend(hmac);
    std::fs::write(path, file).unwrap();
}
```

- [ ] **Step 3: Write the failing tests** (in `read.rs`'s `mod tests`)

```rust
use super::*;
use crate::history::test_archive;
use libsignal_message_backup::proto::backup::{frame, ChatItem, Frame};
use presage::libsignal_service::prelude::Uuid;

const KEY: [u8; 32] = [9; 32];
fn aci() -> Uuid {
    Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
}

fn chat_item_frame(date_sent: u64) -> Frame {
    let mut item = ChatItem::new();
    item.date_sent = date_sent;
    let mut f = Frame::new();
    f.item = Some(frame::Item::ChatItem(item));
    f
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sig-history-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("history.tmp")
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_round_trip() {
    let path = tmp("roundtrip");
    test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1), chat_item_frame(2)]);
    let mut rx = stream(path, KEY, aci());
    let mut seen = vec![];
    while let Some(f) = rx.recv().await {
        seen.push(f.expect("valid archive"));
    }
    let sent: Vec<u64> = seen
        .iter()
        .map(|f| match &f.item {
            Some(frame::Item::ChatItem(c)) => c.date_sent,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(sent, vec![1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flipped_byte_fails_before_any_frame() {
    let path = tmp("flipped");
    test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let mut rx = stream(path, KEY, aci());
    let first = rx.recv().await;
    assert!(matches!(first, Some(Err(_))), "got {first:?}");
    assert!(rx.recv().await.is_none(), "nothing after the error");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_file_fails() {
    let path = tmp("truncated");
    test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    let mut rx = stream(path, KEY, aci());
    assert!(matches!(rx.recv().await, Some(Err(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_key_fails() {
    let path = tmp("wrongkey");
    test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
    let mut rx = stream(path, [1; 32], aci());
    assert!(matches!(rx.recv().await, Some(Err(_))));
}
```

- [ ] **Step 4: Run them** — `cargo test -p sig-core history::read` → FAIL (`stream` missing).

- [ ] **Step 5: Implement read.rs**

```rust
//! Verifies, decrypts and streams a downloaded archive as decoded frames, off the core thread.

use std::path::PathBuf;

use libsignal_message_backup::frame::{FileReaderFactory, FramesReader};
use libsignal_message_backup::parse::VarintDelimitedReader;
use libsignal_message_backup::proto::backup::{BackupInfo, Frame};
use presage::libsignal_service::prelude::Uuid;
use protobuf::Message as _;
use tokio::sync::mpsc;

use crate::history::test_archive; // only the key derivation lives there in tests; see below

/// How many decoded frames may wait between the blocking reader and the importer.
const CHANNEL: usize = 256;

/// Streams the archive's frames. The file's HMAC is verified over the whole file before the
/// first frame is sent. An `Err` item reports a failure (HMAC, decryption, gzip, protobuf) and
/// ends the stream; a closed channel without one is the end of the archive.
pub(crate) fn stream(path: PathBuf, key: [u8; 32], aci: Uuid) -> mpsc::Receiver<Result<Frame, String>> {
    let (tx, rx) = mpsc::channel(CHANNEL);
    // AES + gzip are CPU work: keep them off the core thread.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = futures::executor::block_on(read_all(&path, &key, aci, &tx)) {
            let _ = tx.blocking_send(Err(e));
        }
    });
    rx
}

/// Derives the archive's key from the ephemeral backup key and our ACI (no forward-secrecy
/// token in link-and-sync).
pub(crate) fn message_backup_key(
    key: &[u8; 32],
    aci: Uuid,
) -> libsignal_message_backup::key::MessageBackupKey {
    use presage::libsignal_service::libsignal_account_keys::BackupKey;
    use presage::libsignal_service::protocol::Aci;
    let backup_key = BackupKey(*key);
    let id = backup_key.derive_backup_id(&Aci::from(aci));
    libsignal_message_backup::key::MessageBackupKey::derive(&backup_key, &id, None)
}

async fn read_all(
    path: &std::path::Path,
    key: &[u8; 32],
    aci: Uuid,
    tx: &mpsc::Sender<Result<Frame, String>>,
) -> Result<(), String> {
    let mbk = message_backup_key(key, aci);
    let reader = FramesReader::new(&mbk, FileReaderFactory { path })
        .await
        .map_err(|e| e.to_string())?;
    let mut frames = VarintDelimitedReader::new(reader);
    // The first item is the BackupInfo header.
    let info = frames
        .read_next()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("empty archive")?;
    let info = BackupInfo::parse_from_bytes(&info).map_err(|e| e.to_string())?;
    tracing::info!(version = info.version, "reading history archive");
    while let Some(bytes) = frames.read_next().await.map_err(|e| e.to_string())? {
        let frame = Frame::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
        if tx.blocking_send(Ok(frame)).is_err() {
            return Ok(()); // the importer went away (cancelled): stop reading
        }
    }
    Ok(())
}
```

Notes for the implementer:
- `FileReaderFactory { path }` — check its exact construction in
  `~/.cargo/git/checkouts/libsignal-*/439f035/rust/message-backup/src/frame.rs` (it holds a path
  and opens the file twice: once for the HMAC pass, once for reading).
- `blocking_send` inside `block_on` on a blocking thread is fine; it parks that thread only.
- Deduplicate `message_backup_key`: `test_archive.rs` should call `read::message_backup_key`
  instead of defining its own (fix up the Task 3 Step 2 version accordingly).
- Add to `history/mod.rs`:

```rust
pub(crate) mod read;
#[cfg(test)]
pub(crate) mod test_archive;
```

- [ ] **Step 6: Run them** — `cargo test -p sig-core history::read` → PASS. These tests prove
  compatibility with libsignal's own writer, i.e. the real format.

- [ ] **Step 7: Commit** — `git commit -am "The history archive is verified, decrypted and streamed as frames"`

---

### Task 4: Waiting for and downloading the archive

`history/fetch.rs` long-polls `GET /v1/devices/transfer_archive` and downloads
`{cdn}/attachments/{key}` to `history.tmp`, resuming from the bytes already there. HTTP sits
behind an `ArchiveServer` trait so tests script the server.

**Files:**
- Create: `core/sig-core/src/history/fetch.rs`
- Modify: `core/sig-core/src/history/mod.rs` (`pub(crate) mod fetch;`)

**Interfaces:**
- Consumes: `HistoryImportState`/`HistoryDbStage` (Task 2), `HistoryError` (Task 3).
- Produces:

```rust
pub(crate) enum Polled {
    NotYet,
    Ready { cdn: u32, key: String },
    /// The phone gave up (CONTINUE_WITHOUT_UPLOAD) or wants a fresh relink (RELINK_REQUESTED).
    NoUpload,
}

pub(crate) struct DownloadStart {
    /// Where the returned bytes start; 0 if the server ignored the requested offset.
    pub offset: u64,
    pub total: Option<u64>,
    pub body: std::pin::Pin<Box<dyn futures::io::AsyncRead>>,
}

pub(crate) trait ArchiveServer {
    /// One long poll, waiting up to `timeout_secs` on the server.
    async fn wait(&mut self, timeout_secs: u32) -> Result<Polled, HistoryError>;
    /// Starts (or resumes, from `offset`) the archive download.
    async fn download(&mut self, cdn: u32, key: &str, offset: u64) -> Result<DownloadStart, HistoryError>;
}

pub(crate) enum Fetched { File, NoUpload, Cancelled }

/// Drives wait + download, keeping `history_import` and emitting `HistoryProgress`, until the
/// archive is fully on disk at `tmp` (or there is none, or the user cancelled).
pub(crate) async fn fetch_archive(
    server: &mut impl ArchiveServer,
    db: &crate::projection::ProjectionDb,
    tmp: &std::path::Path,
    l: &dyn crate::events::EventListener,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Fetched, HistoryError>;

/// The production server: authenticated as this device.
pub(crate) struct SignalServer { /* push: PushService */ }
impl SignalServer {
    pub fn from_registration(data: &presage::manager::RegistrationData) -> Result<Self, HistoryError>;
}
```

- Also produces (in mod.rs, used by fetch and later by the driver):

```rust
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStage { Waiting, Downloading, Importing, Done, NoHistory, Failed }
```

and in `events.rs` a new `CoreEvent` variant:

```rust
/// How the history import is doing. `done`/`total` are bytes while `Downloading`
/// (total 0 until known), imported items while `Importing` (total 0: unknown).
HistoryProgress { stage: HistoryStage, done: u64, total: u64 },
```

`lib.rs` re-exports `pub use history::HistoryStage;`.

- [ ] **Step 1: Write the failing tests** (in `fetch.rs`'s `mod tests`; a scripted fake)

```rust
use super::*;
use crate::events::{CoreEvent, EventListener};
use crate::projection::{HistoryDbStage, HistoryImportState, ProjectionDb};
use std::sync::Mutex;

struct Quiet;
impl EventListener for Quiet {
    fn on_event(&self, _: CoreEvent) {}
}

/// Plays a script: each `wait` answer, then serves `file` honoring (or ignoring) offsets.
struct Fake {
    waits: Vec<Result<Polled, HistoryError>>,
    file: Vec<u8>,
    /// Bytes served per download call before the connection "drops" (None: all).
    cut_after: Option<usize>,
    honors_range: bool,
    offsets_seen: Mutex<Vec<u64>>,
}

impl ArchiveServer for Fake {
    async fn wait(&mut self, _timeout: u32) -> Result<Polled, HistoryError> {
        self.waits.remove(0)
    }
    async fn download(&mut self, _cdn: u32, _key: &str, offset: u64) -> Result<DownloadStart, HistoryError> {
        self.offsets_seen.lock().unwrap().push(offset);
        let start = if self.honors_range { offset as usize } else { 0 };
        let end = self.cut_after.map_or(self.file.len(), |n| (start + n).min(self.file.len()));
        Ok(DownloadStart {
            offset: start as u64,
            total: Some(self.file.len() as u64),
            body: Box::pin(futures::io::Cursor::new(self.file[start..end].to_vec())),
        })
    }
}

fn state(key: u8) -> HistoryImportState {
    HistoryImportState {
        stage: HistoryDbStage::Waiting,
        key: vec![key; 32],
        aci: "22222222-2222-2222-2222-222222222222".into(),
        cdn: None,
        cdn_key: None,
        bytes_done: 0,
        total_bytes: None,
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sig-fetch-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("history.tmp");
    let _ = std::fs::remove_file(&path);
    path
}

fn no_cancel() -> tokio::sync::watch::Receiver<bool> {
    tokio::sync::watch::channel(false).1
}

#[tokio::test(start_paused = true)]
async fn waits_until_ready_then_downloads_all() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    db.set_history_state(&state(1)).await.unwrap();
    let mut fake = Fake {
        waits: vec![Ok(Polled::NotYet), Ok(Polled::Ready { cdn: 3, key: "k".into() })],
        file: vec![5u8; 10_000],
        cut_after: None,
        honors_range: true,
        offsets_seen: Mutex::new(vec![]),
    };
    let path = tmp("full");
    let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut no_cancel()).await.unwrap();
    assert!(matches!(got, Fetched::File));
    assert_eq!(std::fs::read(&path).unwrap(), vec![5u8; 10_000]);
    let s = db.history_state().await.unwrap().unwrap();
    assert_eq!((s.stage, s.bytes_done, s.total_bytes), (HistoryDbStage::Importing, 10_000, Some(10_000)));
}

#[tokio::test(start_paused = true)]
async fn a_cut_download_resumes_at_the_right_byte() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    db.set_history_state(&state(1)).await.unwrap();
    let mut fake = Fake {
        waits: vec![Ok(Polled::Ready { cdn: 3, key: "k".into() })],
        file: (0..=255u8).cycle().take(9_000).collect(),
        cut_after: Some(4_000),
        honors_range: true,
        offsets_seen: Mutex::new(vec![]),
    };
    let path = tmp("resume");
    let expected = fake.file.clone();
    let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut no_cancel()).await.unwrap();
    assert!(matches!(got, Fetched::File));
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    assert_eq!(*fake.offsets_seen.lock().unwrap(), vec![0, 4_000, 8_000]);
}

#[tokio::test(start_paused = true)]
async fn a_server_that_ignores_range_restarts_the_file() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    let mut s = state(1);
    s.stage = HistoryDbStage::Downloading;
    s.cdn = Some(3);
    s.cdn_key = Some("k".into());
    s.bytes_done = 4_000; // a previous run got this far
    s.total_bytes = Some(9_000);
    db.set_history_state(&s).await.unwrap();
    let path = tmp("norange");
    std::fs::write(&path, vec![9u8; 4_000]).unwrap(); // stale partial content
    let mut fake = Fake {
        waits: vec![],
        file: vec![7u8; 9_000],
        cut_after: None,
        honors_range: false,
        offsets_seen: Mutex::new(vec![]),
    };
    let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut no_cancel()).await.unwrap();
    assert!(matches!(got, Fetched::File));
    assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 9_000], "no stale prefix");
}

#[tokio::test(start_paused = true)]
async fn a_complete_file_skips_the_network() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    let mut s = state(1);
    s.stage = HistoryDbStage::Downloading;
    s.cdn = Some(3);
    s.cdn_key = Some("k".into());
    s.bytes_done = 5_000;
    s.total_bytes = Some(5_000);
    db.set_history_state(&s).await.unwrap();
    let path = tmp("done");
    std::fs::write(&path, vec![1u8; 5_000]).unwrap();
    let mut fake = Fake {
        waits: vec![Err(HistoryError::Server("must not be called".into()))],
        file: vec![],
        cut_after: None,
        honors_range: true,
        offsets_seen: Mutex::new(vec![]),
    };
    let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut no_cancel()).await.unwrap();
    assert!(matches!(got, Fetched::File));
    assert!(fake.offsets_seen.lock().unwrap().is_empty(), "no download either");
}

#[tokio::test(start_paused = true)]
async fn the_phone_giving_up_ends_the_wait() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    db.set_history_state(&state(1)).await.unwrap();
    let mut fake = Fake {
        waits: vec![Ok(Polled::NoUpload)],
        file: vec![],
        cut_after: None,
        honors_range: true,
        offsets_seen: Mutex::new(vec![]),
    };
    let got = fetch_archive(&mut fake, &db, &tmp("noupload"), &Quiet, &mut no_cancel()).await.unwrap();
    assert!(matches!(got, Fetched::NoUpload));
}

#[tokio::test(start_paused = true)]
async fn a_wait_error_is_retried_and_cancel_ends_it() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    db.set_history_state(&state(1)).await.unwrap();
    let (cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    let mut fake = Fake {
        waits: vec![Err(HistoryError::Server("boom".into())), Ok(Polled::NotYet)],
        file: vec![],
        cut_after: None,
        honors_range: true,
        offsets_seen: Mutex::new(vec![]),
    };
    // after the two scripted answers the Vec is empty; cancel before a third is asked for
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let _ = cancel_tx.send(true);
    });
    let got = fetch_archive(&mut fake, &db, &tmp("cancel"), &Quiet, &mut cancel).await.unwrap();
    assert!(matches!(got, Fetched::Cancelled));
}
```

- [ ] **Step 2: Run them** — `cargo test -p sig-core history::fetch` → FAIL (nothing exists).

- [ ] **Step 3: Implement fetch.rs**

The driver:

```rust
/// One long poll's server-side wait (the HTTP client reads time out at 65 s).
const POLL_SECS: u32 = 60;
/// After a failed poll or cut download, wait this long before trying again.
const RETRY: std::time::Duration = std::time::Duration::from_secs(10);
/// How often download progress is persisted and reported (bytes).
const PROGRESS_EVERY: u64 = 4 * 1024 * 1024;

pub(crate) async fn fetch_archive(
    server: &mut impl ArchiveServer,
    db: &ProjectionDb,
    tmp: &Path,
    l: &dyn EventListener,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Fetched, HistoryError> {
    let mut state = db.history_state().await?.ok_or(HistoryError::Server("no import state".into()))?;
    // Wait for the phone's upload, unless a previous run already learned where it is.
    while state.cdn.is_none() {
        l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::Waiting, done: 0, total: 0 });
        if *cancel.borrow() {
            return Ok(Fetched::Cancelled);
        }
        let polled = tokio::select! {
            p = server.wait(POLL_SECS) => p,
            _ = cancel.changed() => return Ok(Fetched::Cancelled),
        };
        match polled {
            Ok(Polled::Ready { cdn, key }) => {
                state.stage = HistoryDbStage::Downloading;
                state.cdn = Some(cdn);
                state.cdn_key = Some(key);
                db.set_history_state(&state).await?;
            }
            Ok(Polled::NoUpload) => return Ok(Fetched::NoUpload),
            Ok(Polled::NotYet) => {}
            Err(e) => {
                tracing::warn!("waiting for the archive failed: {e}; retrying");
                if pause(RETRY, cancel).await {
                    return Ok(Fetched::Cancelled);
                }
            }
        }
    }
    let (cdn, key) = (state.cdn.unwrap(), state.cdn_key.clone().unwrap());
    // Download with resume until the file is complete.
    loop {
        if state.total_bytes.is_some_and(|t| state.bytes_done >= t && t > 0) {
            state.stage = HistoryDbStage::Importing;
            db.set_history_state(&state).await?;
            return Ok(Fetched::File);
        }
        if *cancel.borrow() {
            return Ok(Fetched::Cancelled);
        }
        match download_once(server, db, tmp, l, cancel, &mut state, cdn, &key).await {
            Ok(true) => {}                       // went around; completeness checked above
            Ok(false) => return Ok(Fetched::Cancelled),
            Err(e) => {
                tracing::warn!("downloading the archive failed: {e}; retrying");
                if pause(RETRY, cancel).await {
                    return Ok(Fetched::Cancelled);
                }
            }
        }
    }
}
```

`download_once` opens `tmp` (create), calls `server.download(cdn, key, state.bytes_done)`,
and if `start.offset != state.bytes_done` truncates the file to `start.offset` (a server that
ignored the range answers offset 0 → the file restarts). Then it seeks to `start.offset` and
copies the body in 64 KiB reads (`futures::io::AsyncReadExt::read`), bumping
`state.bytes_done`, persisting + emitting
`HistoryProgress { stage: Downloading, done, total }` every `PROGRESS_EVERY` bytes and once at
the end. `state.total_bytes` is set from `start.total` when known. A body read error returns
`Err(Download(...))` with progress persisted, so the next round resumes. Checks
`cancel.has_changed()`/`*cancel.borrow()` between reads → `Ok(false)`.
When the body ends and `total` is unknown, treat the file as complete: set
`total_bytes = Some(bytes_done)` and loop (the completeness check exits).
`pause` reuses the select-sleep-or-cancel pattern (local copy; actor's `pause` waits on a
Notify, this one on the watch channel).

File IO: use `std::fs::OpenOptions` + `std::io::Seek`/`Write` directly (the file is local;
writes are small and the core thread tolerates them — the same tradeoff download.rs makes).

The production `SignalServer`:

```rust
use presage::libsignal_service::configuration::{Endpoint, ServiceCredentials, SignalServers};
use presage::libsignal_service::prelude::PushService;
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::libsignal_service::protocol::E164;

pub(crate) struct SignalServer {
    push: PushService,
}

impl SignalServer {
    /// Authenticated as this device. presage keeps the account password private, but
    /// `RegistrationData` serializes it, which is how presage itself persists it.
    pub(crate) fn from_registration(
        data: &presage::manager::registered::RegistrationData,
    ) -> Result<Self, HistoryError> {
        let v = serde_json::to_value(data).map_err(|e| HistoryError::Server(e.to_string()))?;
        let password = v["password"]
            .as_str()
            .ok_or_else(|| HistoryError::Server("no account password".into()))?
            .to_owned();
        let e164: E164 = v["phone_number"] /* adjust: see note below */
            ...
    }
}
```

**Implementer note (credentials):** check how `RegistrationData` serializes `phone_number`
(presage's `registered.rs`; the `PhoneNumber` serde impl) and build
`ServiceCredentials { aci: Some(data.service_ids.aci), pni: Some(data.service_ids.pni), phonenumber, password: Some(password), device_id }`
— `service_ids`, `phone_number` and `device_id` are pub fields; only the password needs the
serde detour. The phonenumber crate is re-exported at
`presage::libsignal_service::prelude::phonenumber`; format with
`data.phone_number.format().mode(phonenumber::Mode::E164).to_string()` then `E164::from_str`.
Then `PushService::new(SignalServers::Production, Some(credentials), "Semaphore")`.
Auth only matters for `wait`; `download` is unauthenticated.

```rust
impl ArchiveServer for SignalServer {
    async fn wait(&mut self, timeout_secs: u32) -> Result<Polled, HistoryError> {
        let resp = self
            .push
            .request(
                reqwest::Method::GET,
                Endpoint::service(format!("/v1/devices/transfer_archive?timeout={timeout_secs}")),
                HttpAuthOverride::NoOverride,
            )
            .map_err(|e| HistoryError::Server(e.to_string()))?
            .send()
            .await
            .map_err(|e| HistoryError::Server(e.to_string()))?;
        match resp.status().as_u16() {
            204 => Ok(Polled::NotYet),
            200 => {
                let v: serde_json::Value =
                    resp.json().await.map_err(|e| HistoryError::Server(e.to_string()))?;
                if let Some(error) = v["error"].as_str() {
                    tracing::warn!(error, "the phone didn't upload the archive");
                    return Ok(Polled::NoUpload);
                }
                match (v["cdn"].as_u64(), v["key"].as_str()) {
                    (Some(cdn), Some(key)) => Ok(Polled::Ready { cdn: cdn as u32, key: key.into() }),
                    _ => Err(HistoryError::Server("unexpected transfer_archive answer".into())),
                }
            }
            s => Err(HistoryError::Server(format!("transfer_archive: HTTP {s}"))),
        }
    }

    async fn download(&mut self, cdn: u32, key: &str, offset: u64) -> Result<DownloadStart, HistoryError> {
        let resp = self
            .push
            .request(
                reqwest::Method::GET,
                Endpoint::cdn(cdn, format!("/attachments/{key}")),
                HttpAuthOverride::Unidentified,
            )
            .map_err(|e| HistoryError::Download(e.to_string()))?
            .header(reqwest::header::RANGE, format!("bytes={offset}-"))
            .send()
            .await
            .map_err(|e| HistoryError::Download(e.to_string()))?;
        ...
    }
}
```

`download` maps: 206 → `offset` as requested, total from `Content-Range`'s `/total`; 200 →
offset 0, total from `Content-Length`; 416 with offset > 0 → the file is already complete
(return an empty body with `offset`, total `Some(offset)`); **404 → the archive is gone from
the CDN (it expired): not retryable.** Give `download` the return type
`Result<Option<DownloadStart>, HistoryError>` with `None` meaning gone, and have
`fetch_archive` turn `None` into `Fetched::NoUpload` (the spec's "404 on the archive" row:
"Your phone didn't send message history", continue linked). The `Fake` server in the tests
then wraps its answers in `Some(...)`, plus one test
`a_vanished_archive_means_no_history` where `download` answers `Ok(None)`. Other statuses →
`Err(Download)`. Body:
`Box::pin(resp.bytes_stream()...)` — reqwest gives a `Stream` of `Bytes`; adapt with
`tokio_util`? No new dep: collect via `futures::TryStreamExt::map_err` +
`futures::stream::TryStreamExt::into_async_read` (the `futures` crate has `into_async_read`
on `TryStream<Ok = impl AsRef<[u8]>>` — map `reqwest::Error` into `std::io::Error` first).
`reqwest` is not a direct dep of sig-core yet: add `reqwest = { version = "0.12", default-features = false }`
to Cargo.toml (version-match the tree: `rg -n 'name = "reqwest"' -A1 core/Cargo.lock`).

- [ ] **Step 4: Run the tests** — `cargo test -p sig-core history::fetch` → PASS.

- [ ] **Step 5: Add the `HistoryStage` enum + `CoreEvent::HistoryProgress`** (as in Interfaces),
  re-export from lib.rs, and make sure `cargo test -p sig-core` passes (uniffi checksums are
  compile-time; no Swift yet).

- [ ] **Step 6: Commit** — `git commit -am "The archive is awaited and downloaded with resume"`

---### Task 5: Mapping frames to rows

`history/import.rs` consumes decoded frames and produces batches of rows for projection.db plus
the seed for presage's store. Pure logic, no IO: everything is unit-testable.

**Files:**
- Create: `core/sig-core/src/history/import.rs`
- Modify: `core/sig-core/src/history/mod.rs` (`pub(crate) mod import;`)

**Interfaces:**
- Consumes: `Frame` (proto), `crate::projection::{Message, MessageKind, MessageStatus, Quote}`.
- Produces:

```rust
pub(crate) const BATCH: usize = 1_000;

#[derive(Default, Debug)]
pub(crate) struct HistoryBatch {
    pub conversations: Vec<ImportConversation>,
    pub messages: Vec<ImportMessage>,
    pub events: Vec<ImportEvent>,
}
impl HistoryBatch {
    pub fn len(&self) -> usize { self.conversations.len() + self.messages.len() + self.events.len() }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportConversation { pub id: String, pub title: String, pub expire_timer: u32 }

#[derive(Debug, PartialEq)]
pub(crate) struct ImportMessage {
    pub message: crate::projection::Message,
    /// (reactor service id, emoji), one per reactor (the item's latest by sortOrder).
    pub reactions: Vec<(String, String)>,
    pub attachments: Vec<ImportAttachment>,
    pub preview: Option<ImportPreview>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportAttachment {
    pub position: i64,
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub is_voice_note: bool,
    /// Encoded `AttachmentPointer` if still downloadable (transit key + uploaded within 90
    /// days); None shows as "no longer available" (state Failed).
    pub pointer: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ImportPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<ImportAttachment>,
}

/// A TimerChanged chat event.
#[derive(Debug, PartialEq)]
pub(crate) struct ImportEvent { pub conversation_id: String, pub at: i64, pub author: String, pub value: i64 }

#[derive(Default, Debug, PartialEq, Clone)]
pub(crate) struct Counts {
    pub messages: u64,
    pub conversations: u64,
    pub skipped: u64,
    pub expired: u64,
    pub unavailable_files: u64,
}

/// What presage's store needs so sending keeps working: contacts (with profile keys) and groups.
#[derive(Default)]
pub(crate) struct Seed {
    pub contacts: Vec<presage::model::contacts::Contact>,
    pub groups: Vec<([u8; 32], presage::model::groups::Group)>,
}

pub(crate) struct Importer { /* me, now_ms, recipient map, chat map, batch, counts, seed parts */ }
impl Importer {
    pub fn new(me_aci: &str, now_ms: i64) -> Self;
    /// Buffers one frame's rows; returns a full batch when `BATCH` items are ready.
    pub fn add(&mut self, frame: Frame) -> Option<HistoryBatch>;
    /// The final partial batch, the presage seed, and the counters.
    pub fn finish(self) -> (HistoryBatch, Seed, Counts);
}
```

**The mapping rules** (each is a test):

| Frame | Rows |
|---|---|
| `Recipient/contact` with ACI | remember id → `c:<aci>`; seed contact (name = profileGivenName + " " + profileFamilyName, trimmed; profile key if 32 bytes) |
| `Recipient/contact` with only PNI | remember id → `c:PNI:<pni>` (no seed contact: presage contacts are ACI-keyed) |
| `Recipient/contact` with only e164 | skipped, counted; its chats and items skipped too |
| `Recipient/self` | id → `c:<me>` |
| `Recipient/group` | id → `g:<hex master key>`; seed group (title/timer/description from snapshot blobs, members from snapshot joined with contact profile keys — unknown ones zeroed) |
| `Recipient/distributionList, callLink, releaseNotes` | skipped, counted |
| `Chat` | `ImportConversation { id: recipient's conversation, title: recipient's title (contacts/groups), expire_timer: expirationTimerMs/1000 }` |
| `ChatItem/standardMessage` | `Message { kind: Normal, body: text.body, quote }` + reactions + attachments + preview |
| `ChatItem/contactMessage` | `MessageKind::Contact { name }` |
| `ChatItem/stickerMessage` | `MessageKind::Sticker { emoji }` |
| `ChatItem/paymentNotification` | `MessageKind::Payment` |
| `ChatItem/giftBadge` | `MessageKind::GiftBadge` |
| `ChatItem/poll` | `MessageKind::Poll { question }` |
| `ChatItem/remoteDeletedMessage`, `adminDeletedMessage` | `deleted: true`, no body |
| `ChatItem/updateMessage` with `expirationTimerChatUpdate` | `ImportEvent { at: dateSent, value: expiresInMs/1000 }` |
| `ChatItem/updateMessage` others, `viewOnceMessage`, `directStoryReplyMessage`, unset | skipped, counted |
| `ChatItem` whose expiry already passed (`expireStartDate + expiresInMs <= now`) | skipped, counted as expired |
| `Frame` unset / `account` / `stickerPack` / `adHocCall` / `notificationProfile` / `chatFolder` | skipped (account isn't counted; the rest count) |

Message field mapping:
- `author`: the authorId recipient — self → `me`, contact → its service-id string (the
  conversation id minus `c:`). Items by other recipients (e.g. a group) are skipped, counted.
- `is_outgoing`: `directionalDetails` is `outgoing`, or author == me.
- `sent_at = dateSent`; `received_at`: incoming → `incoming.dateReceived`, outgoing →
  `outgoing.dateReceived` (or dateSent when 0).
- `status`: incoming → `Read` if `incoming.read` else `Delivered`; outgoing → the max over
  `sendStatus` entries (viewed/read → Read, delivered → Delivered, sent → Sent, pending/skipped
  → Sent, failed → Failed, none → Sent). Never `Pending` for imports (nothing will retry them).
- `edited`: `!revisions.is_empty()` (the top-level item is the newest version; older revisions
  aren't kept — the projection stores only the current body, as in M1).
- `expires_in = expiresInMs / 1000`; `expires_at = expireStartDate + expiresInMs` when
  `expireStartDate > 0`.
- `quote`: only when `targetSentTimestamp` is set and the author is known
  (`Quote { author, sent_at, text: text.body }`); otherwise no quote.
- Reactions: per reactor keep the highest `sortOrder`; reactor = recipient service id (self → me).
- Attachments: `MessageAttachment.pointer` (FilePointer) → downloadable iff
  `locatorInfo.transitCdnKey` is non-empty **and** `transitTierUploadTimestamp >= now - 90 days`;
  then `pointer = Some(AttachmentPointer { attachment_identifier: CdnKey, cdn_number, key, digest (from integrityCheck encryptedDigest), size, content_type, file_name, width, height, upload_timestamp }.encode_to_vec())`
  (prost type `presage::proto::AttachmentPointer`, as projector.rs builds); else `pointer: None`,
  counted in `unavailable_files`. `is_voice_note` = flag == VOICE_MESSAGE.
- Link previews: first `linkPreview` → `ImportPreview`; its image is an attachment at
  position -1 (the convention 0005_attachments.sql documents).

- [ ] **Step 1: Write the failing tests.** One test per table row group, in
  `import.rs`'s `mod tests`. Build frames with the proto types (as in Task 3's tests). The
  essential ones:

```rust
const ME: &str = "11111111-1111-1111-1111-111111111111";
const ALICE: &str = "22222222-2222-2222-2222-222222222222";
const NOW: i64 = 1_800_000_000_000;

fn contact_frame(id: u64, aci: &str, given: &str) -> Frame { /* Recipient { id, contact { aci: uuid bytes, profileKey: [3;32], profileGivenName } } */ }
fn self_frame(id: u64) -> Frame { /* Recipient { id, self {} } */ }
fn chat_frame(id: u64, recipient: u64, timer_ms: u64) -> Frame { /* Chat */ }
fn incoming_text(chat: u64, author: u64, sent_at: u64, body: &str, read: bool) -> Frame { /* ChatItem/standardMessage */ }

#[test]
fn a_contact_chat_and_message_become_rows() {
    let mut imp = Importer::new(ME, NOW);
    assert_eq!(imp.add(contact_frame(1, ALICE, "Alice")), None);
    assert_eq!(imp.add(chat_frame(10, 1, 0)), None);
    assert_eq!(imp.add(incoming_text(10, 1, 5_000, "hi", true)), None);
    let (batch, seed, counts) = imp.finish();
    assert_eq!(batch.conversations, vec![ImportConversation { id: format!("c:{ALICE}"), title: "Alice".into(), expire_timer: 0 }]);
    let m = &batch.messages[0].message;
    assert_eq!((m.conversation_id.as_str(), m.author.as_str(), m.sent_at), (format!("c:{ALICE}").as_str(), ALICE, 5_000));
    assert_eq!((m.is_outgoing, m.status, m.body.as_deref()), (false, MessageStatus::Read, Some("hi")));
    assert_eq!(seed.contacts.len(), 1);
    assert_eq!(seed.contacts[0].name, "Alice");
    assert_eq!(counts.messages, 1);
}

#[test]
fn an_e164_only_contact_and_its_chat_are_skipped() { /* recipient with only e164; chat + item for it; counts.skipped == 3, no rows */ }

#[test]
fn an_expired_item_is_skipped() { /* expireStartDate + expiresInMs < NOW → counts.expired == 1 */ }

#[test]
fn a_still_ticking_timer_is_kept() { /* expires_at = start+len > NOW → message with expires_in/expires_at set */ }

#[test]
fn outgoing_status_is_the_highest_send_status() { /* two sendStatus: delivered + read → Read; failed-only → Failed */ }

#[test]
fn placeholders_map_to_their_kinds() { /* sticker/contact/payment/poll/giftBadge → MessageKind; viewOnce + story reply → skipped */ }

#[test]
fn deleted_messages_are_tombstones() { /* remoteDeletedMessage → deleted: true, no body */ }

#[test]
fn reactions_keep_the_latest_per_reactor() { /* two reactions by same author, higher sortOrder wins */ }

#[test]
fn attachments_follow_the_90_day_rule() {
    /* three attachments: transit key + fresh timestamp → Some(pointer) decodable as
       AttachmentPointer with the cdn key; transit key + 91-day-old stamp → None;
       no transit key → None; counts.unavailable_files == 2 */
}

#[test]
fn a_timer_update_becomes_a_chat_event() { /* updateMessage/expirationTimerChatUpdate → ImportEvent { value: secs } */ }

#[test]
fn a_group_is_seeded_with_members_and_title() {
    /* group frame with snapshot: title blob, timer blob, 2 members (one with a known contact
       profile key, one unknown → zeroed); seed.groups[0].1.title == "...",
       members[0].profile_key is the contact's */
}

#[test]
fn note_to_self_lands_in_our_own_chat() { /* self recipient → c:<ME>; outgoing */ }

#[test]
fn batches_cut_at_the_limit() { /* add 1 contact + 1 chat + BATCH items → add() returns Some once; finish() has the rest */ }

#[test]
fn quotes_come_along() { /* standardMessage with quote { targetSentTimestamp, authorId, text } */ }

#[test]
fn edits_keep_only_the_newest_body() { /* item with revisions → edited: true, body = top-level text */ }
```

- [ ] **Step 2: Run them** — `cargo test -p sig-core history::import` → FAIL.

- [ ] **Step 3: Implement.** Structure:

```rust
enum Known {
    Me,
    Direct { service_id: String },          // "uuid" or "PNI:uuid"
    Group { conversation_id: String },
    Skipped,
}

pub(crate) struct Importer {
    me: String,
    now_ms: i64,
    recipients: std::collections::HashMap<u64, Known>,
    /// Conversation titles by recipient id, for Chat frames that follow.
    titles: std::collections::HashMap<u64, String>,
    chats: std::collections::HashMap<u64, String>, // chat id → conversation id
    profile_keys: std::collections::HashMap<presage::libsignal_service::prelude::Uuid, [u8; 32]>,
    /// Group snapshots wait for finish(), so members can join profile keys seen later.
    group_frames: Vec<([u8; 32], libsignal_message_backup::proto::backup::Group)>,
    contacts: Vec<presage::model::contacts::Contact>,
    batch: HistoryBatch,
    counts: Counts,
}
```

`add` matches `frame.item`; helper fns `recipient()`, `chat()`, `chat_item()` keep it readable.
`conversation_id` for a direct recipient is `format!("c:{service_id}")`; author is the bare
`service_id`. Groups: `crate::projection::ids::group_id(&master_key)`.
Seed contact:

```rust
presage::model::contacts::Contact {
    uuid,
    phone_number: None,
    name,
    verified: Default::default(),
    profile_key: profile_key.map(|k| k.to_vec()).unwrap_or_default(),
    expire_timer: 0,
    expire_timer_version: 1,
    inbox_position: 0,
    avatar: None,
}
```

(Chat timers for presage live in its contact rows too, but presage re-syncs them with each
received message — the projection's `expire_timer` from the Chat frame is what the UI shows.
Set `expire_timer` on the seed contact when the Chat frame carried one: finish() can join
chats back to contacts; keep it simple — a map `timer_by_recipient: HashMap<u64, u32>` filled
by `chat()`, read in `finish()`.)

Group seeding in `finish()`:

```rust
let members = snapshot.members.iter().filter_map(|m| {
    let aci = Uuid::from_slice(&m.user_id).ok()?;
    Some(presage::model::groups::Member {
        aci: aci.into(),
        role: presage::libsignal_service::groups_v2::Role::Default,
        profile_key: ProfileKey::create(self.profile_keys.get(&aci).copied().unwrap_or([0; 32])),
        joined_at_revision: m.joined_at_version,
        label: None,
        label_emoji: None,
    })
}).collect();
```

(**Implementer note:** check `presage::model::groups::Member`'s exact field types — `role` is
`libsignal_service::groups_v2::Role`; blob helpers: a `GroupAttributeBlob`'s oneof `content`
gives title/timer. If `Role::Default` doesn't exist, check the enum's variants in
`groups_v2/model` — it's the value presage's own group sync uses.)

- [ ] **Step 4: Run the tests** — `cargo test -p sig-core history::import` → PASS.

- [ ] **Step 5: Commit** — `git commit -am "Archive frames map to conversations, messages and placeholders"`

---

### Task 6: Writing batches with merge rules

`ProjectionDb::import_history(&HistoryBatch)` writes one batch in one transaction, with the
decision record's merge rules, and refreshes each touched conversation's summary once.

**Files:**
- Modify: `core/sig-core/src/projection/db.rs`

**Interfaces:**
- Consumes: `HistoryBatch` and friends (Task 5).
- Produces: `ProjectionDb::import_history(&self, batch: &crate::history::import::HistoryBatch) -> Result<(), sqlx::Error>`

Merge rules (from the decision record):
- **conversations**: insert; on conflict only fill an empty title and only set `expire_timer`
  if the row is new (presage is the source of truth for live timers; `set_chat_timer` keeps
  winning later).
- **messages**: `INSERT ... ON CONFLICT(author, sent_at) DO UPDATE SET status = MAX(messages.status, excluded.status)`
  — an existing message's content is kept; read status takes the higher. (Safe for the FTS
  triggers: only `UPDATE OF body` re-indexes, and this never touches body.)
- **reactions / attachments / link_previews**: `INSERT OR IGNORE` (what's here wins; the live
  pipeline has newer state).
- **chat events**: insert a TimerChanged only if no event with the same conversation, kind,
  `at` and value exists (same dedupe `Op::SetTimer` uses — follow `apply`'s SetTimer branch).
- After all rows: `refresh_summary` once per distinct touched conversation.
- No per-row events; the caller emits one `ConversationsChanged` at the end of the import.

- [ ] **Step 1: Write the failing tests** (db.rs `mod tests`; build small `HistoryBatch`es by hand)

```rust
fn imported(conv: &str, author: &str, sent_at: i64, body: &str, status: MessageStatus) -> ImportMessage { /* Normal incoming */ }

#[tokio::test]
async fn an_existing_message_is_kept_but_status_rises() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    // live first: Delivered, body "live"
    db.insert_message(&msg("c:a", "a", 5, "live", MessageStatus::Delivered)).await.unwrap();
    // import says: Read, body "archived"
    db.import_history(&batch_with(vec![imported("c:a", "a", 5, "archived", MessageStatus::Read)])).await.unwrap();
    let m = db.message(&key("a", 5)).await.unwrap().unwrap();
    assert_eq!((m.body.as_deref(), m.status), (Some("live"), MessageStatus::Read));
}

#[tokio::test]
async fn a_second_import_changes_nothing() { /* import same batch twice; dump messages+reactions+attachments equal */ }

#[tokio::test]
async fn import_updates_summaries_and_unread() { /* 2 unread incoming → conversations() shows unread 2, preview = newest body */ }

#[tokio::test]
async fn an_imported_title_never_overwrites_a_known_one() { /* existing title "Live" stays; empty one gets filled */ }

#[tokio::test]
async fn imported_reactions_and_attachments_do_not_clobber() { /* existing reaction by same reactor stays; attachment at same position stays */ }

#[tokio::test]
async fn timer_events_are_deduped() { /* import same ImportEvent twice (two calls) → one chat_events row */ }

#[tokio::test]
async fn unavailable_files_are_failed_rows() { /* ImportAttachment { pointer: None } → attachments() state == Failed; with pointer → Pending */ }
```

- [ ] **Step 2: Run them** — FAIL (`import_history` missing).

- [ ] **Step 3: Implement.** One `pool.begin()`; reuse `kind_of`, `refresh_summary`. Messages
  largely mirror `insert_message_in`'s INSERT with the ON CONFLICT clause replacing OR IGNORE,
  minus the per-row `refresh_summary`/`apply_pending` (imports precede receiving, so
  `pending_status` is empty — skip it). Attachment rows:

```sql
INSERT OR IGNORE INTO attachments(message_author, message_sent_at, position, content_type,
  file_name, size, width, height, is_voice_note, pointer, state)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
```

with state `Pending` when `pointer.is_some()` else `Failed`. Collect
`touched: BTreeSet<String>` of conversation ids from all parts; end with
`for id in touched { refresh_summary(&mut tx, &id).await?; }`.

- [ ] **Step 4: Run the tests** — PASS. Full `cargo test -p sig-core` → PASS.

- [ ] **Step 5: The volume test** (`#[ignore]`, run manually; numbers go in the PR body):

```rust
/// 100k messages across 50 chats; run with
/// `cargo test -p sig-core --release -- --ignored volume` and put time and peak RSS in the PR.
#[tokio::test]
#[ignore]
async fn volume_100k_messages() {
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    let started = std::time::Instant::now();
    for page in 0..100 {
        let msgs = (0..1000).map(|i| imported(&format!("c:{:08}-0000-0000-0000-000000000000", page % 50), "a..", (page * 1000 + i) as i64 + 1, "hello world", MessageStatus::Read)).collect();
        db.import_history(&batch_with(msgs)).await.unwrap();
    }
    eprintln!("100k messages in {:?}", started.elapsed());
    assert_eq!(db.conversations().await.unwrap().len(), 50);
}
```

(Adjust author/conversation construction so ids are valid; each conversation id needs the `c:`
prefix and distinct authors per message aren't required — `(author, sent_at)` must be unique,
so vary `sent_at` globally.)

- [ ] **Step 6: Commit** — `git commit -am "Imported history merges into existing data without clobbering it"`

---

### Task 7: The driver, actor and client wiring

`history::run` glues fetch → read → import → seed → cleanup. The actor runs it after linking
(or on launch, resuming) and starts receiving only when it's done. New commands cancel it or
relink; `Client` exposes them over uniffi.

**Files:**
- Modify: `core/sig-core/src/history/mod.rs` (the `run` driver)
- Modify: `core/sig-core/src/actor.rs`
- Modify: `core/sig-core/src/client.rs`
- Modify: `core/sig-core/src/events.rs` (done in Task 4)

**Interfaces:**
- Produces (history/mod.rs):

```rust
pub(crate) enum Outcome { Done, NoHistory, Cancelled }

/// The whole import: wait, download, verify, read, import, seed presage, clean up. Emits
/// `HistoryProgress` and ends with the state row and temp file removed (except on error, where
/// both stay for Retry). On an Archive error the download state resets so Retry re-downloads.
pub(crate) async fn run(
    db: &ProjectionDb,
    store: &mut SqliteStore,
    server: &mut impl fetch::ArchiveServer,
    tmp: &std::path::Path,
    l: &dyn EventListener,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Outcome, HistoryError>
```

- Produces (client.rs, `#[uniffi::export]`):
  - `pub async fn history_pending(&self) -> Result<bool, CoreError>` — a history import is
    waiting/running (drives the app's progress screen on launch).
  - `pub fn cancel_history(&self) -> Result<(), CoreError>` — stop it, delete the archive and
    state, then start receiving ("Continue without history"). Also the Failed-state cleanup.
  - `pub fn retry_history(&self) -> Result<(), CoreError>` — after a failure: try again from
    the saved state (sends `Command::Start`).
  - `pub async fn relink(&self) -> Result<(), CoreError>` — "Transfer message history…": tells
    the server best-effort, stops the core, deletes presage.db and history leftovers, keeps
    projection.db and attachments. The app reopens a `Client` and links again.
- Produces (actor.rs): `Command::CancelHistory`, `Command::Relink { done: oneshot::Sender<()> }`.

- [ ] **Step 1: Write the failing driver test** (history/mod.rs `mod tests`; uses Task 4's Fake
  pattern — move the `Fake` server + `Quiet` listener into a `#[cfg(test)] pub(crate) mod test_support`
  in fetch.rs so both test modules share them)

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_real_archive_imports_end_to_end() {
    let dir = tempdir(); // temp_dir()-based helper as elsewhere
    let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
    let mut store = SqliteStore::open_with_options(
        crate::actor::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
        presage::model::identity::OnNewIdentity::Trust,
    ).await.unwrap();
    // archive: Alice + chat + 2 messages, written with the Task 3 helper
    let key = [9u8; 32];
    let archive = dir.join("archive.bin");
    test_archive::write(&archive, &key, me_uuid(), &frames());
    db.set_history_state(&HistoryImportState { stage: Waiting, key: key.to_vec(), aci: ME.into(), .. }).await.unwrap();
    let mut fake = Fake { waits: vec![Ok(Polled::Ready { cdn: 3, key: "k".into() })], file: std::fs::read(&archive).unwrap(), .. };
    let sink = Sink::default();
    let tmp = dir.join("history.tmp");
    let out = run(&db, &mut store, &mut fake, &tmp, &sink, &mut no_cancel()).await.unwrap();
    assert!(matches!(out, Outcome::Done));
    assert_eq!(db.history_state().await.unwrap(), None, "state row gone");
    assert!(!tmp.exists(), "temp file gone");
    assert_eq!(db.conversations().await.unwrap().len(), 1);
    let contacts: Vec<_> = store.contacts().await.unwrap().collect();
    assert_eq!(contacts.len(), 1, "presage seeded");
    // the progress events ended with Done
    assert!(sink.stages().ends_with(&[HistoryStage::Importing, HistoryStage::Done]) /* adjust: dedupe consecutive */);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_download_resets_for_retry() {
    /* valid state + tmp file with garbage bytes + bytes_done == total so fetch skips the
       network → run returns Err(Archive), tmp deleted, state stage back to Waiting with
       cdn/key kept? NO — reset cdn/key too? The upload is still there; keep cdn/key, reset
       bytes_done/total so Retry re-downloads:
       state.stage == Downloading, bytes_done == 0, total_bytes == None */
}

#[tokio::test(flavor = "multi_thread")]
async fn no_upload_cleans_up_and_reports_no_history() {
    /* Fake answers NoUpload → Outcome::NoHistory, state row gone, last stage NoHistory */
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_cleans_up() {
    /* cancel sent before run → Outcome::Cancelled, state + tmp gone, last stage NoHistory */
}
```

- [ ] **Step 2: Run them** — FAIL.

- [ ] **Step 3: Implement `run`**

```rust
pub(crate) async fn run(
    db: &ProjectionDb,
    store: &mut SqliteStore,
    server: &mut impl fetch::ArchiveServer,
    tmp: &std::path::Path,
    l: &dyn EventListener,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Outcome, HistoryError> {
    let cleanup = |db: &ProjectionDb| async move {
        let _ = std::fs::remove_file(tmp);
        db.clear_history_state().await
    };
    match fetch::fetch_archive(server, db, tmp, l, cancel).await? {
        fetch::Fetched::NoUpload => {
            cleanup(db).await?;
            l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::NoHistory, done: 0, total: 0 });
            l.on_event(CoreEvent::Error { message: "Your phone didn't send message history.".into() });
            return Ok(Outcome::NoHistory);
        }
        fetch::Fetched::Cancelled => {
            cleanup(db).await?;
            l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::NoHistory, done: 0, total: 0 });
            return Ok(Outcome::Cancelled);
        }
        fetch::Fetched::File => {}
    }
    let state = db.history_state().await?.ok_or_else(|| HistoryError::Server("state vanished".into()))?;
    let key: [u8; 32] = state.key.as_slice().try_into().map_err(|_| HistoryError::Archive("bad key".into()))?;
    let aci = Uuid::parse_str(&state.aci).map_err(|e| HistoryError::Archive(e.to_string()))?;
    l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::Importing, done: 0, total: 0 });
    let mut frames = read::stream(tmp.to_path_buf(), key, aci);
    let me = aci.to_string();
    let mut importer = import::Importer::new(&me, chrono::Utc::now().timestamp_millis());
    let mut items: u64 = 0;
    loop {
        if *cancel.borrow() {
            cleanup(db).await?;
            l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::NoHistory, done: 0, total: 0 });
            return Ok(Outcome::Cancelled);
        }
        let Some(next) = frames.recv().await else { break };
        let frame = match next {
            Ok(f) => f,
            Err(e) => {
                // nothing of a bad archive is kept; Retry re-downloads it
                let _ = std::fs::remove_file(tmp);
                let mut reset = state.clone();
                reset.stage = crate::projection::HistoryDbStage::Downloading;
                reset.bytes_done = 0;
                reset.total_bytes = None;
                db.set_history_state(&reset).await?;
                return Err(HistoryError::Archive(e));
            }
        };
        if let Some(batch) = importer.add(frame) {
            items += batch.len() as u64;
            db.import_history(&batch).await?;
            l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::Importing, done: items, total: 0 });
        }
    }
    let (rest, seed, counts) = importer.finish();
    items += rest.len() as u64;
    db.import_history(&rest).await?;
    // What presage needs to send: contacts with profile keys, group master keys. Failures are
    // logged, not fatal: the phone's contact sync fills the same gap.
    for c in &seed.contacts {
        if let Err(e) = presage::store::ContentsStore::save_contact(store, c).await {
            tracing::warn!("seeding a contact failed: {e}");
        }
    }
    for (master_key, group) in seed.groups {
        if let Err(e) = presage::store::ContentsStore::save_group(store, master_key, group).await {
            tracing::warn!("seeding a group failed: {e}");
        }
    }
    tracing::info!(
        messages = counts.messages, conversations = counts.conversations, skipped = counts.skipped,
        expired = counts.expired, unavailable_files = counts.unavailable_files, "history imported"
    );
    cleanup(db).await?;
    l.on_event(CoreEvent::HistoryProgress { stage: HistoryStage::Done, done: items, total: 0 });
    l.on_event(CoreEvent::ConversationsChanged);
    Ok(Outcome::Done)
}
```

(The closure-captures-`tmp` cleanup sketch won't borrow-check as written — make `cleanup` a
small `async fn cleanup(db: &ProjectionDb, tmp: &Path)`.)

- [ ] **Step 4: Run the driver tests** — PASS.

- [ ] **Step 5: Actor wiring (failing test first where testable).**

In `actor.rs`:
- `spawn` passes a clone of the command sender into `run` (new parameter `commands: mpsc::UnboundedSender<Command>`), so tasks can send the actor commands.
- New state in `run()`:

```rust
let history_running = Rc::new(Cell::new(false));
let history_cancel: Rc<RefCell<watch::Sender<bool>>> = Rc::new(RefCell::new(watch::channel(false).0));
```

- `Command::Link` success arm, before `listener.on_event(CoreEvent::Linked)`:

```rust
if let Some(key) = m.registration_data().ephemeral_backup_key() {
    let state = crate::projection::HistoryImportState {
        stage: crate::projection::HistoryDbStage::Waiting,
        key: key.to_vec(),
        aci: m.registration_data().service_ids.aci.to_string(),
        cdn: None,
        cdn_key: None,
        bytes_done: 0,
        total_bytes: None,
    };
    if let Err(e) = projection.set_history_state(&state).await {
        listener.on_event(CoreEvent::Error { message: format!("keeping the history key failed: {e}") });
    }
}
```

- `Command::Start`'s `Some(m)` arm gains a gate at the top:

```rust
Some(m) if !receiving.get() => {
    match projection.history_state().await {
        Ok(Some(_)) => {
            if !history_running.get() {
                start_history(m, &projection, &listener, &dir, &commands, &history_running, &history_cancel);
            }
            continue; // receiving starts when the import is done (the task sends Start)
        }
        Ok(None) => {}
        Err(e) => listener.on_event(CoreEvent::Error { message: format!("checking the history import failed: {e}") }),
    }
    ... existing start code ...
}
```

(`dir` = the data dir: `run()` already gets it as its first parameter; keep a clone.)

- `start_history` (free fn in actor.rs):

```rust
/// Runs the history import on this thread; receiving waits for it. When it ends (imported,
/// none offered, or cancelled) it starts receiving by sending `Start`; a failure waits for
/// the user (Retry → `Start`, Continue → `CancelHistory`).
fn start_history(
    m: &Manager<SqliteStore, Registered>,
    projection: &ProjectionDb,
    listener: &Arc<dyn EventListener>,
    dir: &std::path::Path,
    commands: &mpsc::UnboundedSender<Command>,
    running: &Rc<Cell<bool>>,
    cancel: &Rc<RefCell<watch::Sender<bool>>>,
) {
    running.set(true);
    let (tx, mut rx) = watch::channel(false);
    *cancel.borrow_mut() = tx;
    let (m, db, l, commands, running) = (m.clone(), projection.clone(), listener.clone(), commands.clone(), running.clone());
    let tmp = dir.join("history.tmp");
    tokio::task::spawn_local(async move {
        let mut store = m.store().clone();
        let outcome = async {
            let mut server = crate::history::fetch::SignalServer::from_registration(m.registration_data())?;
            crate::history::run(&db, &mut store, &mut server, &tmp, l.as_ref(), &mut rx).await
        }
        .await;
        running.set(false);
        match outcome {
            Ok(_) => {
                let _ = commands.send(Command::Start);
            }
            Err(e) => {
                l.on_event(CoreEvent::Error { message: format!("the history transfer failed: {e}") });
                l.on_event(CoreEvent::HistoryProgress { stage: crate::history::HistoryStage::Failed, done: 0, total: 0 });
            }
        }
    });
}
```

- New commands:

```rust
/// Stop the history import (or clear a failed one) and continue without it; receiving starts.
CancelHistory,
/// Stops the core so the app can delete presage.db and link again for history. Best effort
/// tells the server to drop the old device registration.
Relink { done: oneshot::Sender<()> },
```

handlers:

```rust
Command::CancelHistory => {
    if history_running.get() {
        let _ = history_cancel.borrow().send(true); // the task cleans up and sends Start
    } else {
        if let Err(e) = projection.clear_history_state().await {
            listener.on_event(CoreEvent::Error { message: format!("clearing the history import failed: {e}") });
        }
        let _ = std::fs::remove_file(dir.join("history.tmp"));
        listener.on_event(CoreEvent::HistoryProgress { stage: crate::history::HistoryStage::NoHistory, done: 0, total: 0 });
        if manager.is_some() {
            let _ = commands.send(Command::Start);
        }
    }
}
Command::Relink { done } => {
    if let Some(m) = &manager {
        // Best effort: the phone's device list replaces this entry at the new link anyway.
        let _ = tokio::time::timeout(Duration::from_secs(10), Box::pin(m.unlink_self())).await;
    }
    linked.store(false, Ordering::SeqCst);
    let _ = done.send(());
    return; // the thread ends; the Client deletes presage.db and reopens
}
```

Note: `Command::Start`'s existing `Some(_) if receiving.get() => {}` arm stays first, so the
gate only runs when not yet receiving.

- [ ] **Step 6: Client API** (client.rs):

```rust
#[uniffi::export]
impl Client {
    /// A history import is waiting, downloading or importing (set right after linking when the
    /// phone offered history; cleared when it finishes or is abandoned).
    ...
    pub fn cancel_history(&self) -> Result<(), CoreError> {
        self.send(Command::CancelHistory)
    }

    /// After a failed transfer: picks the import up from where it stopped.
    pub fn retry_history(&self) -> Result<(), CoreError> {
        self.send(Command::Start)
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    pub async fn history_pending(&self) -> Result<bool, CoreError> {
        Ok(self.db.history_state().await?.is_some())
    }

    /// Relink to transfer history: stops the core and removes presage's store (this Mac becomes
    /// a new linked device) but keeps all shown conversations and files. Open a new `Client`
    /// and `link` afterwards; the import then merges the phone's history in.
    pub async fn relink(&self) -> Result<(), CoreError> {
        let (done, stopped) = tokio::sync::oneshot::channel();
        self.send(Command::Relink { done })?;
        stopped.await.map_err(|_| CoreError::Closed)?;
        drop(self.commands.lock().ok().and_then(|mut c| c.take()));
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            if !matches!(tokio::task::spawn_blocking(move || thread.join()).await, Ok(Ok(()))) {
                tracing::error!("the core thread ended abnormally while relinking");
            }
        }
        // a stale import (old key) must not resume against the new link
        self.db.clear_history_state().await?;
        self.db.close().await;
        let storage = |e: std::io::Error| CoreError::Storage { message: e.to_string() };
        for suffix in ["", "-wal", "-shm"] {
            match std::fs::remove_file(self.data_dir.join(format!("presage.db{suffix}"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(storage(e)),
                _ => {}
            }
        }
        match std::fs::remove_file(self.data_dir.join("history.tmp")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(storage(e)),
            _ => Ok(()),
        }
    }
}
```

- [ ] **Step 7: Integration tests** (client.rs `mod tests`, following the existing temp-dir
  pattern there):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn relink_keeps_the_projection_and_drops_presage() {
    /* open an unlinked Client in a temp dir; write a message into its projection via the db
       handle (or: skip-level — insert via a second ProjectionDb handle is not possible while
       locked, so use client queries: there are none that write... instead: assert files).
       Simplest honest test: create presage.db + history.tmp beforehand is impossible (open
       owns the dir) — so: open client, call relink(), assert presage.db gone,
       projection.db still exists, client is closed (second relink errors Closed). */
}

#[tokio::test(flavor = "multi_thread")]
async fn history_pending_reflects_the_state_row() {
    /* open unlinked client; history_pending() == false. (The true case is covered by db tests
       + driver tests; setting the row needs the db handle, which Client owns privately.) */
}
```

(Keep these two modest — the heavy lifting was tested in Tasks 4–6; the actor's Start gate is
exercised on the real phone per the testing plan.)

- [ ] **Step 8: Run everything** — `cd core && cargo test -p sig-core` → all pass. Then
  `make format && make lint`.

- [ ] **Step 9: Commit** — `git commit -am "History is imported after linking, before receiving starts"`

---

### Task 8: The app: progress, cancel, retry, relink

The app shows the import's progress over the chat screen, offers Cancel / Retry / Continue
without history, and Settings gains "Transfer Message History…" (relink). FakeClient grows the
new protocol methods.

**Files:**
- Run: `make core && make project` (regenerates `app/Generated/sig_core.swift` with the new API — never edit it)
- Modify: `app/Semaphore/AppStore.swift`
- Create: `app/Semaphore/HistoryView.swift`
- Modify: `app/Semaphore/ContentView.swift` (overlay in the `.chats` case)
- Modify: `app/Semaphore/SemaphoreApp.swift` (SettingsView button)
- Modify: `app/Semaphore/LinkView.swift` (one hint line)
- Modify: `app/SemaphoreTests/FakeClient.swift`
- Create: `app/SemaphoreTests/AppStoreHistoryTests.swift`

**Interfaces:**
- Consumes (generated): `CoreEvent.historyProgress(stage: HistoryStage, done: UInt64, total: UInt64)`,
  `client.historyPending()`, `client.cancelHistory()`, `client.retryHistory()`, `client.relink()`.
- Produces (AppStore):

```swift
struct HistoryProgress: Equatable {
    var stage: HistoryStage
    var done: UInt64
    var total: UInt64
}
private(set) var history: HistoryProgress?   // non-nil: the overlay shows
func cancelHistory()
func retryHistory()
func relinkForHistory() async -> String?     // nil: relinked, back at the QR code
```

- [ ] **Step 1: Regenerate bindings** — `make core && make project`. Expect FakeClient to stop
  compiling (new protocol methods): that is this task's "failing test".

- [ ] **Step 2: FakeClient**

```swift
var historyPendingAnswer = false
private(set) var cancelHistoryCalls = 0
private(set) var retryHistoryCalls = 0
private(set) var relinkCalls = 0

func historyPending() async throws -> Bool { historyPendingAnswer }
func cancelHistory() throws { cancelHistoryCalls += 1 }
func retryHistory() throws { retryHistoryCalls += 1 }
func relink() async throws { relinkCalls += 1 }
```

(match the FakeClient file's existing style — it records calls for other methods already).

- [ ] **Step 3: Write the failing Swift tests** (`AppStoreHistoryTests.swift`, in the style of
  the existing AppStoreTests: make a store with a FakeClient opener, boot, feed events via
  `store.handle(...)`)

```swift
@MainActor
final class AppStoreHistoryTests: XCTestCase {
    func testProgressEventsDriveTheOverlay() async {
        let (store, _) = makeLinkedStore()  // the file's helper: FakeClient, boot, .chats
        await store.boot()
        XCTAssertNil(store.history)
        store.handle(.historyProgress(stage: .waiting, done: 0, total: 0))
        XCTAssertEqual(store.history, .init(stage: .waiting, done: 0, total: 0))
        store.handle(.historyProgress(stage: .downloading, done: 512, total: 1024))
        XCTAssertEqual(store.history?.stage, .downloading)
        store.handle(.historyProgress(stage: .importing, done: 2000, total: 0))
        XCTAssertEqual(store.history?.done, 2000)
        store.handle(.historyProgress(stage: .done, done: 2000, total: 0))
        XCTAssertNil(store.history, "done dismisses the overlay")
    }

    func testNoHistoryDismissesAndFailureStays() async { /* .noHistory → nil; .failed → stays with stage failed */ }

    func testBootShowsTheOverlayWhenAnImportIsPending() async {
        /* fake.historyPendingAnswer = true before boot → after boot, history != nil (waiting) */
    }

    func testCancelAndRetryReachTheClient() async { /* cancelHistory() → fake.cancelHistoryCalls == 1; retry too */ }

    func testRelinkKeepsTheKeyAndShowsTheCode() async {
        /* relinkForHistory() → fake.relinkCalls == 1, forgetKey NOT called (inject a flag like
           existing tests do for forgetKey), screen == .link after reboot with an unlinked fake */
    }
}
```

- [ ] **Step 4: Run them** — unsigned test run:

```
xcodebuild -project app/Semaphore.xcodeproj -scheme Semaphore -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath build test CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY="" \
  -only-testing:SemaphoreTests/AppStoreHistoryTests
```

Expected: FAIL (no `history` property yet).

- [ ] **Step 5: AppStore implementation**

- the struct + property (above), plus in `handle(_:)`:

```swift
case .historyProgress(let stage, let done, let total):
    Self.logger.notice("history \(String(describing: stage), privacy: .public)")
    switch stage {
    case .done:
        history = nil
        refreshConversations()
    case .noHistory:
        history = nil
    default:
        history = HistoryProgress(stage: stage, done: done, total: total)
    }
```

- in `boot()`'s linked branch, after `try client.start()`:

```swift
if (try? await client.historyPending()) == true, history == nil {
    history = HistoryProgress(stage: .waiting, done: 0, total: 0)
}
```

- actions:

```swift
/// "Continue without history": stops the import; messages keep working.
func cancelHistory() {
    do { try client?.cancelHistory() } catch { showError("Couldn't stop the transfer: \(Self.describe(error))") }
}

func retryHistory() {
    history = HistoryProgress(stage: .waiting, done: 0, total: 0)
    do { try client?.retryHistory() } catch { showError("Couldn't retry the transfer: \(Self.describe(error))") }
}

/// Settings ▸ Transfer Message History…: this Mac relinks as a new device; everything shown
/// is kept and the phone's history merges in. Returns why it couldn't start, or nil.
func relinkForHistory() async -> String? {
    guard let client else { return "Semaphore isn't running." }
    do { try await client.relink() } catch {
        Self.logger.error("relink failed: \(Self.describe(error), privacy: .private)")
        return Self.describe(error)
    }
    await restart(keepKey: true)
    return nil
}
```

- `restart()` becomes `restart(keepKey: Bool = false)`; it calls `forgetKey()` and
  `notifier?.removeAll()` only when `!keepKey` (the data and its key stay for a relink — the
  conversations on disk are kept by design).

- [ ] **Step 6: HistoryView**

`app/Semaphore/HistoryView.swift` — a card overlay (Desktop's link-and-sync look: progress +
stage text), shown by ContentView:

```swift
import SwiftUI

/// The history transfer's progress over the chat list, with a way out at every stage.
struct HistoryView: View {
    let progress: AppStore.HistoryProgress
    let cancel: () -> Void
    let retry: () -> Void

    private var title: String {
        switch progress.stage {
        case .waiting: "Preparing to transfer…"
        case .downloading: "Transferring your messages…"
        case .importing: "Importing your messages…"
        case .failed: "The transfer didn't finish"
        case .done, .noHistory: ""
        }
    }

    private var detail: String? {
        switch progress.stage {
        case .waiting: "Keep Signal open on your phone."
        case .downloading where progress.total > 0:
            let done = Double(progress.done) / 1_048_576
            let total = Double(progress.total) / 1_048_576
            return String(format: "%.0f of %.0f MB", done, total)
        case .importing where progress.done > 0: "\(progress.done.formatted()) items"
        case .failed: "Your chats keep working; new messages arrive once you continue."
        default: nil
        }
    }

    var body: some View {
        VStack(spacing: 14) {
            if progress.stage == .failed {
                Image(systemName: "exclamationmark.triangle").font(.largeTitle).foregroundStyle(.secondary)
            } else if progress.stage == .downloading, progress.total > 0 {
                ProgressView(value: Double(progress.done), total: Double(progress.total))
                    .frame(width: 220)
            } else {
                ProgressView()
            }
            Text(title).font(.headline)
            if let detail { Text(detail).font(.callout).foregroundStyle(.secondary) }
            if progress.stage == .failed {
                HStack {
                    Button("Continue Without History", action: cancel)
                    Button("Retry", action: retry).buttonStyle(.borderedProminent)
                }
            } else {
                Button("Continue Without History", action: cancel)
            }
        }
        .padding(28)
        .frame(maxWidth: 380)
        .background(Theme.window, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(.separator))
    }
}
```

ContentView's `.chats` case wraps the `NavigationSplitView` in a `ZStack`:

```swift
ZStack {
    NavigationSplitView { ... existing ... }
    if let progress = core.history {
        Color.black.opacity(0.35).ignoresSafeArea()
        HistoryView(progress: progress, cancel: core.cancelHistory, retry: core.retryHistory)
    }
}
```

LinkView: after the instructions Text, add:

```swift
Text("If your phone offers to transfer your message history, accept it to see your older messages here.")
    .font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 360)
```

SettingsView (`SemaphoreApp.swift`), in the `store.screen == .chats` section under the Unlink
row:

```swift
LabeledContent("Message history") {
    Button("Transfer Message History…") { confirmRelink = true }.disabled(working)
}
```

with `@State private var confirmRelink = false` and a second confirmationDialog:

```swift
.confirmationDialog("Transfer message history from your phone?", isPresented: $confirmRelink) {
    Button("Relink and Transfer") { relink() }
} message: {
    Text("This Mac is relinked as a new device: you'll scan a QR code again, and your phone "
        + "will offer to transfer its message history. Everything already here is kept.")
}
```

```swift
private func relink() {
    working = true
    Task {
        if let failed = await store.relinkForHistory() { store.showError(failed) }
        working = false
    }
}
```

- [ ] **Step 7: Run the Swift tests** — the AppStoreHistoryTests command from Step 4 → PASS;
  then the full test suite (drop `-only-testing:`) → all pass.

- [ ] **Step 8: Look at it** — `make run` is blocked (no signing certificate on this machine);
  build ad-hoc instead and run:

```
xcodebuild -project app/Semaphore.xcodeproj -scheme Semaphore -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath build -configuration Debug build CODE_SIGN_IDENTITY="-" CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM=""
open build/Build/Products/Debug/Semaphore.app
```

The app opens at the QR screen (this machine is unlinked): check the new LinkView hint renders.
The progress overlay itself renders in previews/tests; the full flow needs the owner's phone.

- [ ] **Step 9: Commit** — `git commit -am "The app shows the history transfer and offers relinking for it"`

---

### Task 9: Spec update, checks, PR and review

- [ ] **Step 1: Update the spec** (`docs/superpowers/specs/2026-10-16-m2-history-sync-design.md`)
  where implementation diverged, at least:
  - the reader uses `libsignal-message-backup` with the `test-util` feature (public `proto` +
    `export`), not a prost copy of backup.proto — the open question about dependency
    unification is answered (it unifies);
  - long polls are 60 s (the HTTP client's 65 s read timeout), looping indefinitely with
    Cancel always available (no separate 1-h budget state);
  - `capabilities=backup5` is always on the QR code (first link asks too; declining on the
    phone is a normal link);
  - cancel/continue auto-starts receiving.

- [ ] **Step 2: Full verification** — `make format && make check` (runs Rust + Swift tests,
  lint). Fix anything. Run the volume test and note the numbers:
  `cd core && cargo test -p sig-core --release -- --ignored volume 2>&1 | tail -3`.

- [ ] **Step 3: Commit, push, PR**

```bash
git push -u origin feat/history-sync
gh pr create --title "Message history transfers from the phone when Semaphore is linked" --body "$(cat <<'EOF'
## What
- The linking QR code asks the phone for message history (link-and-sync, capabilities=backup5).
- After linking, the core waits for the phone's encrypted archive, downloads it with resume,
  verifies its HMAC, decrypts it and imports conversations, messages, reactions, attachments
  and timer events straight into projection.db. Contacts, profile keys and group keys seed
  presage's store. Receiving starts only after the import, so nothing interleaves.
- Relinking ("Transfer Message History…" in Settings) keeps everything on this Mac and merges
  the phone's history in; existing rows are never overwritten (status only rises).
- Progress UI with Cancel, Retry and Continue Without History; a crash or quit resumes.
- Media: files still on the transit CDN (uploaded within 90 days) download as usual; older
  ones show as no longer available.

## Why
M2: the app starts empty today; this brings the phone's history over. Decisions in
docs/decisions/2026-10-16-history-import-merges.md.

## How it was tested
- Rust: reader round-trips real-format archives (written with libsignal's own export
  helpers), HMAC/truncation/wrong-key failures, download resume + Range-ignoring servers +
  crash resume, every frame mapping, merge rules and idempotency, end-to-end driver runs.
- Volume: 100k messages import in <time> (<peak memory>).
- Swift: progress overlay state machine, cancel/retry/relink plumbing, boot with a pending
  import.
- On-phone testing (relink with history, declining, cancel on the phone, quit mid-download)
  is the owner's checklist before merge.

Closes #18
EOF
)"
```

- [ ] **Step 4: Card** — `make card issue=18 status="In review"` (currently fails: gh lacks the
  project scope; note it in chat instead of failing the task).

- [ ] **Step 5: Review** — run `/pr-review-toolkit:review-pr` on `git diff origin/main...HEAD`
  per CLAUDE.md step 7; fix every Critical/Important on this branch, re-run checks, push, post
  one PR comment listing each finding and its fix or ruling.
