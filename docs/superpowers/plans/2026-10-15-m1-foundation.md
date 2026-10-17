# M1 Foundation Implementation Plan (Plan 1 of 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the entire stack end to end: a Rust core on presage, exposed to a SwiftUI Mac app through UniFFI, that links to Pitis's phone by QR, receives messages, projects them into a queryable encrypted DB, and shows them in a raw event log.

**Architecture:** presage's `Manager`/`SqliteStore` are `!Send`, so the core runs one dedicated OS thread with a current-thread tokio runtime + `LocalSet` (the "actor"). The UniFFI `Client` object is `Send + Sync` and only holds a command channel into the actor. Received `Content` goes through a **pure projector** (`Content → Vec<Op>`) and then into a separate **projection DB** (own sqlx SQLite/SQLCipher file — separate from presage's file because both use `sqlx::migrate!` and would collide on `_sqlx_migrations`). Events stream back to Swift via a UniFFI callback interface.

**Tech Stack:** Rust 1.96, presage @ `971b14bf1b7382e45bc363966e1ed588173e7c36`, presage-store-sqlite (bundled SQLCipher), sqlx 0.9, tokio 1.5x, uniffi 0.32; Swift 6 / SwiftUI / AppKit, macOS 15+, Xcode 27, XcodeGen 2.46.

**Spec:** `docs/superpowers/specs/2026-10-15-m1-daily-driver-design.md`

**Plan 2** (features + real UI: sending, reactions/edits/deletes, attachments, timeline, composer, notifications, search, packaging) is written after this plan lands, against the real APIs discovered here. All M1 work is tracked as GitHub issues in `pitis/hush`.

## Global Constraints

- arm64 only, macOS 15.0 deployment target.
- presage pinned to rev `971b14bf1b7382e45bc363966e1ed588173e7c36`; bump only deliberately.
- No `cdsi` feature (avoids BoringSSL/OpenSSL conflict).
- Must replicate presage's `[patch.crates-io]` for `libsqlite3-sys` (whisperfish fork, rev `2a42b3354c9194700d08aa070f70a131a470e7dc`).
- Never call presage from outside the actor thread.
- DB key: 32 random bytes hex-encoded, stored in macOS Keychain (service `dev.pitis.hush`, account `db-key`); same key for presage store and projection DB.
- Data dir: `~/Library/Application Support/Hush/` (`presage.db`, `projection.db`, `attachments/`).
- Device name shown on phone: `Hush`.
- No "Signal" branding in app name/icon. App name: Hush. Bundle id `dev.pitis.hush`.
- Conversation ids: `c:<aci-uuid>` for 1:1, `g:<hex master key>` for groups. Message identity = `(author_aci, sent_at_ms)`.

## Review Focus

1. **Messages sent from Pitis's phone** arrive as `SyncMessage.sent`, not `DataMessage` — must land in the right 1:1/group conversation as outgoing. (Test in Task 3.)
2. **Duplicate delivery** of the same `(author, sent_at)` (reconnect, replay) must not create a second row or double-count unread. (Test in Task 2.)
3. **Reaction/edit/delete arriving before its target** (out-of-order) must not crash or error the receive loop; the op is a no-op for now. (Test in Task 4.)
4. **FTS5 availability in bundled SQLCipher** — if missing, search silently breaks later. (Test in Task 2.)
5. **Unknown/unsupported content** (calls, stories, null messages, decryption errors) must be skipped, never panic the actor. (Test in Task 3.)

---

### Task 1: Scaffold — Rust core + UniFFI + XcodeGen app that calls Rust

**Files:**
- Create: `.gitignore`, `README.md`, `Makefile`, `CLAUDE.md`, `scripts/{worktree,update,card}.sh`, `.githooks/`
- Create: `core/Cargo.toml`, `core/sig-core/Cargo.toml`, `core/sig-core/src/lib.rs`
- Create: `core/uniffi-bindgen/Cargo.toml`, `core/uniffi-bindgen/src/main.rs`
- Create: `scripts/build-core.sh`
- Create: `app/project.yml`, `app/Hush/HushApp.swift`, `app/Hush/ContentView.swift`

**Interfaces:**
- Produces: `sig_core::core_version() -> String` (UniFFI-exported → Swift `coreVersion()`); `app/Generated/sig_core.swift` + `app/Frameworks/SigCore.xcframework` produced by `scripts/build-core.sh`.

- [ ] **Step 1: Install tools**

```bash
brew install protobuf xcodegen
```

- [ ] **Step 2: Workspace manifests**

`core/Cargo.toml`:
```toml
[workspace]
members = ["sig-core", "uniffi-bindgen"]
resolver = "2"

[workspace.dependencies]
uniffi = "0.32"

[patch.crates-io]
libsqlite3-sys = { version = "0.36.0", git = "https://github.com/whisperfish/rusqlite", rev = "2a42b3354c9194700d08aa070f70a131a470e7dc" }

[profile.release]
lto = "thin"
strip = true
```

`core/sig-core/Cargo.toml`:
```toml
[package]
name = "sig-core"
version = "0.1.0"
edition = "2021"
license = "AGPL-3.0-only"

[lib]
crate-type = ["staticlib", "lib"]

[dependencies]
uniffi = { workspace = true }
presage = { git = "https://github.com/whisperfish/presage", rev = "971b14bf1b7382e45bc363966e1ed588173e7c36" }
presage-store-sqlite = { git = "https://github.com/whisperfish/presage", rev = "971b14bf1b7382e45bc363966e1ed588173e7c36" }
sqlx = { version = "0.9", features = ["sqlite", "runtime-tokio", "migrate"] }
tokio = { version = "1.48", features = ["rt", "sync", "time", "macros"] }
futures = "0.3"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
hex = "0.4"
chrono = "0.4"

[dev-dependencies]
tokio = { version = "1.48", features = ["rt", "macros"] }
```

`core/uniffi-bindgen/Cargo.toml`:
```toml
[package]
name = "uniffi-bindgen"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
uniffi = { workspace = true, features = ["cli"] }
```

`core/uniffi-bindgen/src/main.rs`:
```rust
fn main() {
    uniffi::uniffi_bindgen_main()
}
```

- [ ] **Step 3: Write the failing test** in `core/sig-core/src/lib.rs`:

```rust
uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::core_version(), env!("CARGO_PKG_VERSION"));
    }
}
```

- [ ] **Step 4: Run it, expect compile failure** (`core_version` not found)

Run: `cd core && cargo test -p sig-core`
Expected: FAIL `cannot find function core_version`. (First run also fetches presage/libsignal — takes several minutes; a build failure here in presage deps means the patch section is wrong, fix before continuing.)

- [ ] **Step 5: Implement**

```rust
/// Version of the Rust core, shown in the app's About panel.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
```

- [ ] **Step 6: Run test, expect PASS**

Run: `cd core && cargo test -p sig-core`

- [ ] **Step 7: Build script** (as built, `scripts/build-core.sh` also maps `debug`→Cargo profile `dev`, which the version below misses, and asserts a single modulemap) `scripts/build-core.sh` (chmod +x):

```bash
#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${1:-release}"
cd "$ROOT/core"
cargo build -p sig-core --target aarch64-apple-darwin --profile "$PROFILE"
LIB="$ROOT/core/target/aarch64-apple-darwin/$PROFILE/libsig_core.a"
GEN="$ROOT/app/Generated"; HDR="$ROOT/core/target/uniffi-headers"
rm -rf "$GEN" "$HDR" "$ROOT/app/Frameworks/SigCore.xcframework"
mkdir -p "$GEN" "$HDR" "$ROOT/app/Frameworks"
cargo run -q -p uniffi-bindgen -- generate --library "$LIB" --language swift --out-dir "$GEN"
mv "$GEN"/*.h "$HDR/"
mv "$GEN"/*.modulemap "$HDR/module.modulemap"
xcodebuild -create-xcframework -library "$LIB" -headers "$HDR" \
  -output "$ROOT/app/Frameworks/SigCore.xcframework" >/dev/null
echo "built SigCore.xcframework ($PROFILE)"
```

Run: `rustup target add aarch64-apple-darwin && ./scripts/build-core.sh debug`
Expected: `app/Generated/sig_core.swift` exists and contains `func coreVersion()`.

- [ ] **Step 8: XcodeGen project** (as built, `app/project.yml` links Security/SystemConfiguration as `sdk:` dependencies, generates Info.plist and signs the test target ad hoc; the repo is authoritative) `app/project.yml`:

```yaml
name: Hush
options:
  bundleIdPrefix: dev.pitis
  deploymentTarget: { macOS: "15.0" }
settings:
  base:
    SWIFT_VERSION: "6.0"
    ARCHS: arm64
    OTHER_LDFLAGS: ["-framework", "Security", "-framework", "SystemConfiguration"]
targets:
  Hush:
    type: application
    platform: macOS
    sources: [Hush, Generated]
    dependencies:
      - framework: Frameworks/SigCore.xcframework
        embed: false
    info:
      path: Hush/Info.plist
      properties:
        CFBundleDisplayName: Hush
        LSMinimumSystemVersion: "15.0"
    settings:
      base:
        PRODUCT_BUNDLE_IDENTIFIER: dev.pitis.hush
        CODE_SIGN_IDENTITY: "-"
        ENABLE_HARDENED_RUNTIME: NO
  HushTests:
    type: bundle.unit-test
    platform: macOS
    sources: [HushTests]
    dependencies: [{ target: Hush }]
```

`app/Hush/HushApp.swift`:
```swift
import SwiftUI

@main
struct HushApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}
```

`app/Hush/ContentView.swift`:
```swift
import SwiftUI

struct ContentView: View {
    var body: some View {
        Text("Hush — core \(coreVersion())").padding()
    }
}
```

Create `app/HushTests/SmokeTests.swift`:
```swift
import XCTest
@testable import Hush

final class SmokeTests: XCTestCase {
    func testCoreVersionIsNotEmpty() {
        XCTAssertFalse(coreVersion().isEmpty)
    }
}
```

- [ ] **Step 9: Build + test the app**

Run: `cd app && xcodegen && xcodebuild -project Hush.xcodeproj -scheme Hush -destination 'platform=macOS,arch=arm64' test | tail -5`
Expected: `** TEST SUCCEEDED **`. (Linker errors for missing system frameworks → add them to `OTHER_LDFLAGS`; the libsignal stack may need `-framework CoreFoundation`.)

- [ ] **Step 10: Makefile, .gitignore, README** (as built: a Makefile replaces the justfile, and `.gitignore` differs; the repo is authoritative)

`justfile`:
```make
core:
    ./scripts/build-core.sh debug
app: core
    cd app && xcodegen && xcodebuild -project Hush.xcodeproj -scheme Hush -configuration Debug -destination 'platform=macOS,arch=arm64' build
test:
    cd core && cargo test
    cd app && xcodegen && xcodebuild -project Hush.xcodeproj -scheme Hush -destination 'platform=macOS,arch=arm64' test
```

`.gitignore`:
```
core/target/
app/Generated/
app/Frameworks/
app/Hush.xcodeproj/
DerivedData/
.DS_Store
```

`README.md`: one paragraph (unofficial personal native macOS Signal client; Rust core on presage + SwiftUI; AGPL-3.0; `make check`, `make run`).

- [ ] **Step 11: Commit**

```bash
git add -A && git commit -m "feat: scaffold Rust core, UniFFI bindings and XcodeGen Mac app"
```

---

### Task 2: Projection DB — schema, upserts, queries, FTS5

**Files:**
- Create: `core/sig-core/migrations/0001_init.sql`
- Create: `core/sig-core/src/projection/mod.rs`, `core/sig-core/src/projection/db.rs`, `core/sig-core/src/projection/model.rs`
- Modify: `core/sig-core/src/lib.rs` (add `mod projection;` and re-exports)

> **As built (PR #22, after review):** `upsert_conversation_title(id, title)` derives the kind from the id (`ConversationKind::from_id`, bad ids are `sqlx::Error::InvalidArgument`); `messages(conversation_id, before: Option<MessageKey>, limit)` pages by `(sent_at, author)`; `MessageStatus::from_i64` returns `Option`; `Message::key()`; `refresh_summary(&mut conn, conversation_id)` recomputes preview/last-at/unread from messages and **every later write (Task 4 `apply`) must call it inside its transaction**; deleted messages are stored with `body`/`quote_text` NULL (enforced by CHECK); statement logging is disabled (the key pragma would leak). The code below is the original draft; the repo is authoritative.

**Interfaces:**
- Produces (in `projection::model`, all `#[derive(uniffi::Record, Clone, Debug, PartialEq)]`):
  - `Conversation { id: String, kind: ConversationKind, title: String, last_message_at: i64, last_message_preview: String, unread_count: u32 }`
  - `ConversationKind` (`uniffi::Enum`): `Direct`, `Group`
  - `Message { conversation_id: String, author: String, sent_at: i64, received_at: i64, body: Option<String>, is_outgoing: bool, status: MessageStatus, edited: bool, deleted: bool, quote: Option<Quote> }`
  - `Quote { author: String, sent_at: i64, text: Option<String> }`
  - `MessageStatus` (`uniffi::Enum`): `Failed`, `Pending`, `Sent`, `Delivered`, `Read` (stored as i64 -1..3)
- Produces (in `projection::db`): `ProjectionDb` with
  - `async fn open(url: &str, key: Option<&str>) -> Result<ProjectionDb, sqlx::Error>`
  - `async fn upsert_conversation_title(&self, id: &str, kind: ConversationKind, title: &str) -> Result<(), sqlx::Error>`
  - `async fn insert_message(&self, m: &Message) -> Result<bool, sqlx::Error>` (false if duplicate)
  - `async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error>` (ordered by last_message_at desc)
  - `async fn messages(&self, conversation_id: &str, before: Option<i64>, limit: u32) -> Result<Vec<Message>, sqlx::Error>` (newest-first page, returned oldest→newest)
  - `async fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>, sqlx::Error>`

- [ ] **Step 1: Migration** `core/sig-core/migrations/0001_init.sql`:

```sql
CREATE TABLE conversations (
  id TEXT PRIMARY KEY,
  kind INTEGER NOT NULL,            -- 0 direct, 1 group
  title TEXT NOT NULL DEFAULT '',
  last_message_at INTEGER NOT NULL DEFAULT 0,
  last_message_preview TEXT NOT NULL DEFAULT '',
  unread_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversations(id),
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  body TEXT,
  is_outgoing INTEGER NOT NULL,
  status INTEGER NOT NULL,
  edited INTEGER NOT NULL DEFAULT 0,
  deleted INTEGER NOT NULL DEFAULT 0,
  quote_author TEXT,
  quote_sent_at INTEGER,
  quote_text TEXT,
  UNIQUE (author, sent_at)
);
CREATE INDEX messages_by_conv ON messages(conversation_id, sent_at);

CREATE TABLE reactions (
  target_author TEXT NOT NULL,
  target_sent_at INTEGER NOT NULL,
  reactor TEXT NOT NULL,
  emoji TEXT NOT NULL,
  PRIMARY KEY (target_author, target_sent_at, reactor)
);

CREATE VIRTUAL TABLE messages_fts USING fts5(body, content='messages', content_rowid='id');
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER messages_au AFTER UPDATE OF body ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
```

- [ ] **Step 2: Model** `core/sig-core/src/projection/model.rs` — the records/enums from Interfaces, plus:

```rust
impl MessageStatus {
    pub fn to_i64(self) -> i64 {
        match self { Self::Failed => -1, Self::Pending => 0, Self::Sent => 1, Self::Delivered => 2, Self::Read => 3 }
    }
    pub fn from_i64(v: i64) -> Self {
        match v { -1 => Self::Failed, 0 => Self::Pending, 1 => Self::Sent, 2 => Self::Delivered, _ => Self::Read }
    }
}
```
(`MessageStatus` also derives `Copy, Eq, PartialOrd, Ord` in declaration order Failed<Pending<Sent<Delivered<Read`.)

- [ ] **Step 3: Write failing tests** at the bottom of `db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::model::*;

    async fn db() -> ProjectionDb { ProjectionDb::open("sqlite::memory:", None).await.unwrap() }

    fn msg(author: &str, sent_at: i64, body: &str) -> Message {
        Message { conversation_id: "c:alice".into(), author: author.into(), sent_at, received_at: sent_at,
            body: Some(body.into()), is_outgoing: false, status: MessageStatus::Delivered,
            edited: false, deleted: false, quote: None }
    }

    #[tokio::test]
    async fn insert_and_page_messages() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        for t in 1..=5 { db.insert_message(&msg("alice", t, &format!("m{t}"))).await.unwrap(); }
        let page = db.messages("c:alice", Some(5), 2).await.unwrap();
        assert_eq!(page.iter().map(|m| m.sent_at).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[tokio::test]
    async fn duplicate_message_is_ignored_and_not_double_counted() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        assert!(db.insert_message(&msg("alice", 10, "hi")).await.unwrap());
        assert!(!db.insert_message(&msg("alice", 10, "hi")).await.unwrap());
        let convs = db.conversations().await.unwrap();
        assert_eq!(convs[0].unread_count, 1);
        assert_eq!(convs[0].last_message_preview, "hi");
        assert_eq!(convs[0].last_message_at, 10);
    }

    #[tokio::test]
    async fn conversations_ordered_by_recency() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        db.upsert_conversation_title("c:bob", "Bob").await.unwrap();
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&Message { conversation_id: "c:bob".into(), ..msg("bob", 2, "b") }).await.unwrap();
        let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c:bob", "c:alice"]);
    }

    #[tokio::test]
    async fn fts5_search_finds_words() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        db.insert_message(&msg("alice", 1, "lunch tomorrow at noon")).await.unwrap();
        db.insert_message(&msg("alice", 2, "unrelated")).await.unwrap();
        let hits = db.search("tomorrow", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sent_at, 1);
    }

    #[tokio::test]
    async fn outgoing_message_does_not_increment_unread() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        db.insert_message(&Message { is_outgoing: true, ..msg("me", 1, "yo") }).await.unwrap();
        assert_eq!(db.conversations().await.unwrap()[0].unread_count, 0);
    }
}
```

- [ ] **Step 4: Run, expect FAIL** (`ProjectionDb` undefined)

Run: `cd core && cargo test -p sig-core projection`

- [ ] **Step 5: Implement `db.rs`**

```rust
use std::str::FromStr;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;
use super::model::*;

#[derive(Clone)]
pub struct ProjectionDb { pool: SqlitePool }

impl ProjectionDb {
    pub async fn open(url: &str, key: Option<&str>) -> Result<Self, sqlx::Error> {
        let mut opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true);
        if let Some(k) = key { opts = opts.pragma("key", format!("\"x'{k}'\"")); }
        // in-memory DBs are per-connection: keep a single connection
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn upsert_conversation_title(&self, id: &str, kind: ConversationKind, title: &str) -> Result<(), sqlx::Error> {
        let kind = matches!(kind, ConversationKind::Group) as i64;
        sqlx::query("INSERT INTO conversations(id, kind, title) VALUES (?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET title = CASE WHEN ?3 = '' THEN title ELSE ?3 END")
            .bind(id).bind(kind).bind(title).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn insert_message(&self, m: &Message) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let kind = m.conversation_id.starts_with("g:") as i64;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(&m.conversation_id).bind(kind).execute(&mut *tx).await?;
        let res = sqlx::query(
            "INSERT OR IGNORE INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
               status, edited, deleted, quote_author, quote_sent_at, quote_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)")
            .bind(&m.conversation_id).bind(&m.author).bind(m.sent_at).bind(m.received_at).bind(&m.body)
            .bind(m.is_outgoing).bind(m.status.to_i64()).bind(m.edited).bind(m.deleted)
            .bind(m.quote.as_ref().map(|q| q.author.clone())).bind(m.quote.as_ref().map(|q| q.sent_at))
            .bind(m.quote.as_ref().and_then(|q| q.text.clone()))
            .execute(&mut *tx).await?;
        let inserted = res.rows_affected() == 1;
        if inserted {
            sqlx::query(
                "UPDATE conversations SET
                   last_message_at = MAX(last_message_at, ?2),
                   last_message_preview = CASE WHEN ?2 >= last_message_at THEN COALESCE(?3, '') ELSE last_message_preview END,
                   unread_count = unread_count + ?4
                 WHERE id = ?1")
                .bind(&m.conversation_id).bind(m.sent_at).bind(&m.body).bind(!m.is_outgoing as i64)
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error> {
        let rows = sqlx::query("SELECT * FROM conversations ORDER BY last_message_at DESC")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Conversation {
            id: r.get("id"),
            kind: if r.get::<i64, _>("kind") == 1 { ConversationKind::Group } else { ConversationKind::Direct },
            title: r.get("title"),
            last_message_at: r.get("last_message_at"),
            last_message_preview: r.get("last_message_preview"),
            unread_count: r.get::<i64, _>("unread_count") as u32,
        }).collect())
    }

    pub async fn messages(&self, conversation_id: &str, before: Option<i64>, limit: u32) -> Result<Vec<Message>, sqlx::Error> {
        let mut rows = sqlx::query("SELECT * FROM messages WHERE conversation_id = ?1 AND sent_at < ?2
                                    ORDER BY sent_at DESC LIMIT ?3")
            .bind(conversation_id).bind(before.unwrap_or(i64::MAX)).bind(limit as i64)
            .fetch_all(&self.pool).await?;
        rows.reverse();
        Ok(rows.iter().map(row_to_message).collect())
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>, sqlx::Error> {
        // quote the user's text as one FTS5 string so punctuation can't break the query syntax
        let fts = format!("\"{}\"", query.replace('"', "\"\""));
        let rows = sqlx::query("SELECT m.* FROM messages_fts f JOIN messages m ON m.id = f.rowid
                                WHERE messages_fts MATCH ?1 AND m.deleted = 0 ORDER BY m.sent_at DESC LIMIT ?2")
            .bind(fts).bind(limit as i64).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_message).collect())
    }

    pub(crate) fn pool(&self) -> &SqlitePool { &self.pool }
}

fn row_to_message(r: &SqliteRow) -> Message {
    let quote_sent_at: Option<i64> = r.get("quote_sent_at");
    Message {
        conversation_id: r.get("conversation_id"),
        author: r.get("author"),
        sent_at: r.get("sent_at"),
        received_at: r.get("received_at"),
        body: r.get("body"),
        is_outgoing: r.get("is_outgoing"),
        status: MessageStatus::from_i64(r.get("status")),
        edited: r.get("edited"),
        deleted: r.get("deleted"),
        quote: quote_sent_at.map(|sent_at| Quote { author: r.get("quote_author"), sent_at, text: r.get("quote_text") }),
    }
}
```

`projection/mod.rs`: `pub mod db; pub mod model; pub use db::ProjectionDb; pub use model::*;`

- [ ] **Step 6: Run, expect PASS** (all 5). If `fts5_search_finds_words` fails with `no such module: fts5`, enable it: add `libsqlite3-sys = { version = "0.36", features = ["bundled-sqlcipher"] }` and set env `LIBSQLITE3_FLAGS="-DSQLITE_ENABLE_FTS5"` in `core/.cargo/config.toml` `[env]`; re-run.

Run: `cd core && cargo test -p sig-core projection`

- [ ] **Step 7: Commit** — `git commit -am "feat(core): projection DB with paging, dedupe and FTS5 search"` (add new files first).

---

### Task 3: Projector — text messages (incoming, synced-from-phone, groups) and skipping the rest

> **As built (PR #23, after review):** `Op` has only `InsertMessage` here; Task 4 adds the rest. The projector no longer uses `Thread::try_from`. A message's own group context wins. Otherwise: incoming → `c:<sender>`, own DataMessage → `c:<metadata.destination>`, Sent transcript → `c:<parse_destination_service_id()>`, dropped when there is no usable destination (never Note to Self). Sync messages are only accepted from `me`. Direct ids use `ServiceId::service_id_string()`, so a PNI peer is `c:PNI:<uuid>` (merging PNI→ACI chats is later work). Story replies and view-once are skipped (M4); stickers etc. are skipped pending #24. Author ids go through `ids::aci(string, binary)`; Task 4 must use it (or libsignal's `parse_target_author_aci`) for reaction targets. Test helpers: `content`, `content_to`, `text`, `in_group`, `sync`, `sent_sync` in `projector.rs`.

**Files:**
- Create: `core/sig-core/src/projection/projector.rs`
- Create: `core/sig-core/src/projection/ids.rs`
- Modify: `core/sig-core/src/projection/mod.rs`

**Interfaces:**
- Consumes: `Message`, `Quote`, `MessageStatus` from Task 2; `presage::libsignal_service::content::{Content, ContentBody}`, `presage::store::Thread`.
- Produces:
  - `ids::conversation_id(thread: &Thread) -> String` (`c:<uuid>` / `g:<hex>`)
  - `projector::Op` enum: `InsertMessage(Message)`, `React { target_author: String, target_sent_at: i64, reactor: String, emoji: Option<String> }` (None = remove), `Edit { author: String, target_sent_at: i64, new_body: Option<String> }`, `Delete { author: String, target_sent_at: i64 }`, `Receipt { author_of_targets: String, sent_ats: Vec<i64>, status: MessageStatus }`, `Typing { conversation_id: String, who: String, started: bool }`
  - `projector::project(content: &Content, me: &str) -> Vec<Op>` where `me` = own ACI uuid string.

- [ ] **Step 1: Write failing tests** in `projector.rs` (test helpers build `Content` directly):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::{Content, ContentBody, Metadata};
    use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
    use presage::proto::{sync_message, DataMessage, GroupContextV2, SyncMessage, NullMessage};
    use presage::libsignal_service::prelude::Uuid;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    fn sid(u: &str) -> ServiceId { ServiceId::from(Aci::from(Uuid::parse_str(u).unwrap())) }

    // If DeviceId/Metadata constructors differ at the pinned libsignal rev, adapt this helper only.
    fn content(sender: &str, ts: u64, body: ContentBody) -> Content {
        let t = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap();
        Content {
            metadata: Metadata {
                sender: sid(sender), destination: sid(ME), sender_device: DeviceId::new(1).unwrap(),
                pni_verified: None, client_timestamp: t, server_timestamp: t, needs_receipt: false,
                unidentified_sender: false, was_plaintext: false, server_guid: None,
            },
            body,
        }
    }

    fn text(ts: u64, body: &str) -> DataMessage {
        DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() }
    }

    #[test]
    fn incoming_direct_text() {
        let ops = project(&content(ALICE, 100, ContentBody::DataMessage(text(100, "hi"))), ME);
        let [Op::InsertMessage(m)] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert_eq!(m.author, ALICE);
        assert_eq!(m.sent_at, 100);
        assert_eq!(m.body.as_deref(), Some("hi"));
        assert!(!m.is_outgoing);
    }

    #[test]
    fn sent_from_my_phone_lands_in_recipient_conversation_as_outgoing() {
        let sync = SyncMessage {
            content: Some(sync_message::Content::Sent(sync_message::Sent {
                destination_service_id: Some(ALICE.into()),
                timestamp: Some(200),
                message: Some(text(200, "from phone")),
                ..Default::default()
            })),
            ..Default::default()
        };
        let ops = project(&content(ME, 200, ContentBody::SynchronizeMessage(sync)), ME);
        let [Op::InsertMessage(m)] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(m.conversation_id, format!("c:{ALICE}"));
        assert_eq!(m.author, ME);
        assert!(m.is_outgoing);
        assert_eq!(m.status, MessageStatus::Sent);
    }

    #[test]
    fn group_text_uses_group_conversation() {
        let mut dm = text(300, "group hi");
        dm.group_v2 = Some(GroupContextV2 { master_key: Some(vec![7u8; 32]), ..Default::default() });
        let ops = project(&content(ALICE, 300, ContentBody::DataMessage(dm)), ME);
        let [Op::InsertMessage(m)] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(m.conversation_id, format!("g:{}", hex::encode([7u8; 32])));
    }

    #[test]
    fn quote_is_carried() {
        let mut dm = text(400, "reply");
        dm.quote = Some(presage::proto::data_message::Quote {
            id: Some(100), author_aci: Some(ME.into()), text: Some("orig".into()), ..Default::default() });
        let ops = project(&content(ALICE, 400, ContentBody::DataMessage(dm)), ME);
        let [Op::InsertMessage(m)] = ops.as_slice() else { panic!() };
        assert_eq!(m.quote, Some(Quote { author: ME.into(), sent_at: 100, text: Some("orig".into()) }));
    }

    #[test]
    fn unsupported_content_is_skipped() {
        let ops = project(&content(ALICE, 500, ContentBody::NullMessage(NullMessage::default())), ME);
        assert!(ops.is_empty());
        // empty data message (e.g. profile key update) also yields nothing
        let ops = project(&content(ALICE, 501, ContentBody::DataMessage(DataMessage::default())), ME);
        assert!(ops.is_empty());
    }
}
```

- [ ] **Step 2: Run, expect FAIL** (`project` undefined). Fix any helper constructor compile errors against the real libsignal types first (only the helper, not the assertions).

Run: `cd core && cargo test -p sig-core projector`

- [ ] **Step 3: Implement** `ids.rs`:

```rust
use presage::store::Thread;

pub fn conversation_id(thread: &Thread) -> String {
    match thread {
        Thread::Contact(sid) => format!("c:{}", sid.raw_uuid()),
        Thread::Group(key) => format!("g:{}", hex::encode(key)),
    }
}
```

`projector.rs`:
```rust
use presage::libsignal_service::content::{Content, ContentBody};
use presage::proto::{sync_message, DataMessage};
use presage::store::Thread;
use super::ids::conversation_id;
use super::model::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    InsertMessage(Message),
    React { target_author: String, target_sent_at: i64, reactor: String, emoji: Option<String> },
    Edit { author: String, target_sent_at: i64, new_body: Option<String> },
    Delete { author: String, target_sent_at: i64 },
    Receipt { author_of_targets: String, sent_ats: Vec<i64>, status: MessageStatus },
    Typing { conversation_id: String, who: String, started: bool },
}

pub fn project(content: &Content, me: &str) -> Vec<Op> {
    let sender = content.metadata.sender.raw_uuid().to_string();
    let received_at = content.metadata.server_timestamp.timestamp_millis();
    let Ok(thread) = Thread::try_from(content) else { return vec![] };
    let conv = conversation_id(&thread);
    match &content.body {
        ContentBody::DataMessage(dm) => data_message(dm, &conv, &sender, received_at, sender == me),
        ContentBody::SynchronizeMessage(sm) => match &sm.content {
            Some(sync_message::Content::Sent(sent)) => sent.message.as_ref()
                .map(|dm| data_message(dm, &conv, me, received_at, true))
                .unwrap_or_default(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn data_message(dm: &DataMessage, conv: &str, author: &str, received_at: i64, outgoing: bool) -> Vec<Op> {
    let Some(sent_at) = dm.timestamp.map(|t| t as i64) else { return vec![] };
    if dm.body.is_none() && dm.attachments.is_empty() { return vec![]; }
    vec![Op::InsertMessage(Message {
        conversation_id: conv.to_string(),
        author: author.to_string(),
        sent_at,
        received_at,
        body: dm.body.clone(),
        is_outgoing: outgoing,
        status: if outgoing { MessageStatus::Sent } else { MessageStatus::Delivered },
        edited: false,
        deleted: false,
        quote: dm.quote.as_ref().and_then(|q| Some(Quote {
            author: q.author_aci.clone()?,
            sent_at: q.id? as i64,
            text: q.text.clone(),
        })),
    })]
}
```

Note: for a 1:1 *incoming* message, `Thread::try_from` returns `Contact(sender)`; for our own synced sends it returns `Contact(destination)` — exactly what the tests assert. If the `sync_message::Content` path differs at the pinned rev (e.g. field `sent` instead of oneof), adapt the match arm and the test's construction together.

- [ ] **Step 4: Run, expect PASS** (5 tests). Run: `cd core && cargo test -p sig-core projector`

- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(core): project text messages incl. phone-synced sends and groups"`

---

### Task 4: Projector + DB — reactions, edits, deletes, receipts, typing, and `apply`

> **As built (PR #25, after review):**
> - **Ops:** `React { target: MessageKey, reactor, emoji: Option }`, `Edit { target, new_body, edited_at }`, `Delete { target }`, `SetStatus { targets: Vec<MessageKey>, status }` (receipts, plus the phone's read sync `SyncMessage.read`), and `Typing` (conversation from `direct_id`).
> - **Apply:** `ProjectionDb::apply(&Op) -> Vec<Applied>` with `Applied::{MessageChanged { conversation_id, key }, Typing { .. }}`. Each op is one transaction. Every op that changes a `messages` row calls `refresh_summary`; reactions don't affect the summary. A repeated op reports no change.
> - **Targets may name an edit revision:** Signal clients address an edited message by its latest revision's timestamp. Migration 0002 adds `message_revisions`, and every op resolves its target through it. Edits are ordered by `edited_at`.
> - **Ops arriving before their message:**
>   - a delete is remembered (`pending_deletes`) and the message is stored already deleted;
>   - read state is remembered (`pending_status`);
>   - a reaction is stored anyway;
>   - an edit is dropped (rare).
> - **Delete** clears the body, quote text, reactions and other messages' quotes of it. Reactions on deleted messages are dropped.
> - **Ignored:** story reactions, typing in groups, and **admin deletes** (`DataMessage.adminDelete`). Admin deletes need group admin state to verify the sender (later ticket).
> - **Receipts** aren't scoped to the sender's conversation. Accepted for M1: a receipt needs the exact ms timestamp, and it matches Desktop's "highest status" display in groups.
> - **Read paths:** `message(&MessageKey)` and `reactions(&MessageKey)`.

**Files:**
- Modify: `core/sig-core/src/projection/projector.rs` (extend `project`)
- Modify: `core/sig-core/src/projection/db.rs` (add `apply`, `reactions_for`)
- Modify: `core/sig-core/src/projection/model.rs` (add `Reaction`)

**Interfaces:**
- Consumes: `Op` (Task 3), `ProjectionDb` (Task 2).
- Produces:
  - `Reaction { reactor: String, emoji: String }` (`uniffi::Record`)
  - `ProjectionDb::apply(&self, op: &Op) -> Result<Applied, sqlx::Error>` where `enum Applied { Nothing, MessageChanged { conversation_id: String, author: String, sent_at: i64 }, Typing { conversation_id: String, who: String, started: bool } }`
  - `ProjectionDb::reactions_for(&self, author: &str, sent_at: i64) -> Result<Vec<Reaction>, sqlx::Error>`
  - `ProjectionDb::message(&self, author: &str, sent_at: i64) -> Result<Option<Message>, sqlx::Error>`

- [ ] **Step 1: Failing projector tests** (append to projector tests):

```rust
    use presage::proto::{data_message, receipt_message, EditMessage, ReceiptMessage, TypingMessage, typing_message};

    #[test]
    fn reaction_add_and_remove() {
        let mut dm = DataMessage { timestamp: Some(600), ..Default::default() };
        dm.reaction = Some(data_message::Reaction { emoji: Some("👍".into()), remove: Some(false),
            target_author_aci: Some(ME.into()), target_sent_timestamp: Some(100), ..Default::default() });
        let ops = project(&content(ALICE, 600, ContentBody::DataMessage(dm.clone())), ME);
        assert_eq!(ops, vec![Op::React { target_author: ME.into(), target_sent_at: 100, reactor: ALICE.into(), emoji: Some("👍".into()) }]);
        dm.reaction.as_mut().unwrap().remove = Some(true);
        let ops = project(&content(ALICE, 601, ContentBody::DataMessage(dm)), ME);
        assert!(matches!(&ops[0], Op::React { emoji: None, .. }));
    }

    #[test]
    fn delete_for_everyone() {
        let dm = DataMessage { timestamp: Some(700), delete: Some(data_message::Delete { target_sent_timestamp: Some(100) }), ..Default::default() };
        let ops = project(&content(ALICE, 700, ContentBody::DataMessage(dm)), ME);
        assert_eq!(ops, vec![Op::Delete { author: ALICE.into(), target_sent_at: 100 }]);
    }

    #[test]
    fn edit_message() {
        let em = EditMessage { target_sent_timestamp: Some(100), data_message: Some(text(800, "fixed")) };
        let ops = project(&content(ALICE, 800, ContentBody::EditMessage(em)), ME);
        assert_eq!(ops, vec![Op::Edit { author: ALICE.into(), target_sent_at: 100, new_body: Some("fixed".into()) }]);
    }

    #[test]
    fn read_receipt_targets_my_messages() {
        let rm = ReceiptMessage { r#type: Some(receipt_message::Type::Read as i32), timestamp: vec![100, 101] };
        let ops = project(&content(ALICE, 900, ContentBody::ReceiptMessage(rm)), ME);
        assert_eq!(ops, vec![Op::Receipt { author_of_targets: ME.into(), sent_ats: vec![100, 101], status: MessageStatus::Read }]);
    }

    #[test]
    fn typing_started() {
        let tm = TypingMessage { timestamp: Some(1000), action: Some(typing_message::Action::Started as i32), group_id: None };
        let ops = project(&content(ALICE, 1000, ContentBody::TypingMessage(tm)), ME);
        assert_eq!(ops, vec![Op::Typing { conversation_id: format!("c:{ALICE}"), who: ALICE.into(), started: true }]);
    }
```

Note: `Thread::try_from` may reject receipt/typing content; compute the conversation for those from the sender (`c:<sender>`, or `g:` from `TypingMessage.group_id` is a *group id*, not master key — for M1 group typing indicators are dropped) **before** the `Thread::try_from` early-return. Restructure `project` so only `DataMessage`/`SynchronizeMessage`/`EditMessage` require a thread.

- [ ] **Step 2: Run, expect FAIL.** Run: `cd core && cargo test -p sig-core projector`

- [ ] **Step 3: Implement** — replace `project` with:

```rust
pub fn project(content: &Content, me: &str) -> Vec<Op> {
    let sender = content.metadata.sender.raw_uuid().to_string();
    let received_at = content.metadata.server_timestamp.timestamp_millis();
    match &content.body {
        ContentBody::ReceiptMessage(rm) => {
            use presage::proto::receipt_message::Type;
            let status = match rm.r#type.and_then(|t| Type::try_from(t).ok()) {
                Some(Type::Delivery) => MessageStatus::Delivered,
                Some(Type::Read) | Some(Type::Viewed) => MessageStatus::Read,
                None => return vec![],
            };
            vec![Op::Receipt { author_of_targets: me.into(), sent_ats: rm.timestamp.iter().map(|t| *t as i64).collect(), status }]
        }
        ContentBody::TypingMessage(tm) if tm.group_id.is_none() => {
            use presage::proto::typing_message::Action;
            let started = tm.action == Some(Action::Started as i32);
            vec![Op::Typing { conversation_id: format!("c:{sender}"), who: sender, started }]
        }
        ContentBody::EditMessage(em) => match (em.target_sent_timestamp, &em.data_message) {
            (Some(target), Some(dm)) => vec![Op::Edit { author: sender, target_sent_at: target as i64, new_body: dm.body.clone() }],
            _ => vec![],
        },
        ContentBody::DataMessage(_) | ContentBody::SynchronizeMessage(_) => {
            let Ok(thread) = Thread::try_from(content) else { return vec![] };
            let conv = conversation_id(&thread);
            match &content.body {
                ContentBody::DataMessage(dm) => data_message(dm, &conv, &sender, received_at, sender == me),
                ContentBody::SynchronizeMessage(sm) => match &sm.content {
                    Some(sync_message::Content::Sent(sent)) => sent.message.as_ref()
                        .map(|dm| data_message(dm, &conv, me, received_at, true)).unwrap_or_default(),
                    _ => vec![],
                },
                _ => unreachable!(),
            }
        }
        _ => vec![],
    }
}
```

and at the top of `data_message`, before the empty-body check:

```rust
    if let Some(r) = &dm.reaction {
        return match (&r.target_author_aci, r.target_sent_timestamp) {
            (Some(target_author), Some(ts)) => vec![Op::React {
                target_author: target_author.clone(), target_sent_at: ts as i64, reactor: author.into(),
                emoji: if r.remove == Some(true) { None } else { r.emoji.clone() } }],
            _ => vec![],
        };
    }
    if let Some(d) = &dm.delete {
        return d.target_sent_timestamp.map(|ts| vec![Op::Delete { author: author.into(), target_sent_at: ts as i64 }]).unwrap_or_default();
    }
```

(Edits synced from Pitis's phone arrive as `SyncMessage.sent.edit_message`; add that arm: `Some(sync_message::Content::Sent(sent)) if sent.edit_message.is_some()` → `Op::Edit { author: me, .. }`.)

- [ ] **Step 4: Run projector tests, expect PASS.**

- [ ] **Step 5: Failing DB `apply` tests** (append to db tests):

```rust
    use crate::projection::projector::Op;

    #[tokio::test]
    async fn apply_reaction_edit_delete_receipt() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
        db.insert_message(&Message { is_outgoing: true, status: MessageStatus::Sent, ..msg("me", 100, "orig") }).await.unwrap();

        db.apply(&Op::React { target_author: "me".into(), target_sent_at: 100, reactor: "alice".into(), emoji: Some("👍".into()) }).await.unwrap();
        assert_eq!(db.reactions_for("me", 100).await.unwrap(), vec![Reaction { reactor: "alice".into(), emoji: "👍".into() }]);
        db.apply(&Op::React { target_author: "me".into(), target_sent_at: 100, reactor: "alice".into(), emoji: None }).await.unwrap();
        assert!(db.reactions_for("me", 100).await.unwrap().is_empty());

        db.apply(&Op::Edit { author: "me".into(), target_sent_at: 100, new_body: Some("edited".into()) }).await.unwrap();
        let m = db.message("me", 100).await.unwrap().unwrap();
        assert_eq!((m.body.as_deref(), m.edited), (Some("edited"), true));
        assert_eq!(db.search("edited", 5).await.unwrap().len(), 1);
        assert!(db.search("orig", 5).await.unwrap().is_empty());

        db.apply(&Op::Receipt { author_of_targets: "me".into(), sent_ats: vec![100], status: MessageStatus::Read }).await.unwrap();
        db.apply(&Op::Receipt { author_of_targets: "me".into(), sent_ats: vec![100], status: MessageStatus::Delivered }).await.unwrap();
        assert_eq!(db.message("me", 100).await.unwrap().unwrap().status, MessageStatus::Read); // never downgrades

        db.apply(&Op::Delete { author: "me".into(), target_sent_at: 100 }).await.unwrap();
        let m = db.message("me", 100).await.unwrap().unwrap();
        assert!(m.deleted && m.body.is_none());
    }

    #[tokio::test]
    async fn ops_on_unknown_target_are_noops() {
        let db = db().await;
        let r = db.apply(&Op::Edit { author: "x".into(), target_sent_at: 1, new_body: Some("y".into()) }).await.unwrap();
        assert!(matches!(r, Applied::Nothing));
        db.apply(&Op::Delete { author: "x".into(), target_sent_at: 1 }).await.unwrap();
        db.apply(&Op::Receipt { author_of_targets: "x".into(), sent_ats: vec![1], status: MessageStatus::Read }).await.unwrap();
    }
```

- [ ] **Step 6: Run, expect FAIL.**

- [ ] **Step 7: Implement in `db.rs`:**

```rust
use super::projector::Op;

#[derive(Debug)]
pub enum Applied {
    Nothing,
    MessageChanged { conversation_id: String, author: String, sent_at: i64 },
    Typing { conversation_id: String, who: String, started: bool },
}

impl ProjectionDb {
    pub async fn apply(&self, op: &Op) -> Result<Applied, sqlx::Error> {
        match op {
            Op::InsertMessage(m) => Ok(if self.insert_message(m).await? {
                Applied::MessageChanged { conversation_id: m.conversation_id.clone(), author: m.author.clone(), sent_at: m.sent_at }
            } else { Applied::Nothing }),
            Op::React { target_author, target_sent_at, reactor, emoji } => {
                match emoji {
                    Some(e) => sqlx::query("INSERT INTO reactions VALUES (?1, ?2, ?3, ?4)
                                            ON CONFLICT DO UPDATE SET emoji = ?4")
                        .bind(target_author).bind(target_sent_at).bind(reactor).bind(e).execute(&self.pool).await?,
                    None => sqlx::query("DELETE FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2 AND reactor = ?3")
                        .bind(target_author).bind(target_sent_at).bind(reactor).execute(&self.pool).await?,
                };
                self.changed(target_author, *target_sent_at).await
            }
            Op::Edit { author, target_sent_at, new_body } => {
                sqlx::query("UPDATE messages SET body = ?3, edited = 1 WHERE author = ?1 AND sent_at = ?2 AND deleted = 0")
                    .bind(author).bind(target_sent_at).bind(new_body).execute(&self.pool).await?;
                self.changed(author, *target_sent_at).await
            }
            Op::Delete { author, target_sent_at } => {
                sqlx::query("UPDATE messages SET body = NULL, deleted = 1 WHERE author = ?1 AND sent_at = ?2")
                    .bind(author).bind(target_sent_at).execute(&self.pool).await?;
                self.changed(author, *target_sent_at).await
            }
            Op::Receipt { author_of_targets, sent_ats, status } => {
                let mut last = Applied::Nothing;
                for ts in sent_ats {
                    sqlx::query("UPDATE messages SET status = MAX(status, ?3) WHERE author = ?1 AND sent_at = ?2")
                        .bind(author_of_targets).bind(ts).bind(status.to_i64()).execute(&self.pool).await?;
                    if let a @ Applied::MessageChanged { .. } = self.changed(author_of_targets, *ts).await? { last = a; }
                }
                Ok(last)
            }
            Op::Typing { conversation_id, who, started } =>
                Ok(Applied::Typing { conversation_id: conversation_id.clone(), who: who.clone(), started: *started }),
        }
    }

    async fn changed(&self, author: &str, sent_at: i64) -> Result<Applied, sqlx::Error> {
        Ok(match self.message(author, sent_at).await? {
            Some(m) => Applied::MessageChanged { conversation_id: m.conversation_id, author: m.author, sent_at: m.sent_at },
            None => Applied::Nothing,
        })
    }

    pub async fn message(&self, author: &str, sent_at: i64) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM messages WHERE author = ?1 AND sent_at = ?2")
            .bind(author).bind(sent_at).fetch_optional(&self.pool).await?;
        Ok(row.as_ref().map(row_to_message))
    }

    pub async fn reactions_for(&self, author: &str, sent_at: i64) -> Result<Vec<Reaction>, sqlx::Error> {
        let rows = sqlx::query("SELECT reactor, emoji FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2 ORDER BY reactor")
            .bind(author).bind(sent_at).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Reaction { reactor: r.get("reactor"), emoji: r.get("emoji") }).collect())
    }
}
```

The `Receipt` test relies on the `messages_au` trigger only firing for `body` updates; the delete path updates body to NULL, so FTS also drops the row's text (deleted messages never show in search — `search` also filters `deleted = 0`).

- [ ] **Step 8: Run all core tests, expect PASS.** Run: `cd core && cargo test -p sig-core`

- [ ] **Step 9: Commit** — `git commit -am "feat(core): reactions, edits, deletes, receipts and typing projection"`

---

### Task 5: Actor + UniFFI `Client` — open, link, receive loop, events, queries

> **As built (PR #26, after review):**
> - **Swift API:** `Client.open(dataDir:dbKeyHex:listener:)`, `isLinked()`, `link(deviceName:) throws`, `start() throws`, async `conversations()`, `messages(conversationId:before: MessageKey?, limit:)`, `search(query:limit:)`, `reactions(key:)`.
> - **Errors:** `CoreError` is `Storage`, `AlreadyOpen` or `Closed`.
> - **Open:**
>   - takes a file lock on `<dataDir>/.lock` (a second open → `AlreadyOpen`);
>   - opens both DBs with the raw hex key, with statement logging off;
>   - treats only presage's `NotYetRegisteredError` as unlinked; any other load error fails `open`, because linking clears the registration.
> - **Drop:** dropping the `Client` joins the core thread.
> - **Link:** times out after 5 min (`Error` + `Unlinked`). A failed `request_contacts` is reported as an `Error`.
> - **Receiving:**
>   - the receive loop is supervised (a panic → `Error` + `Offline`, and `start` works again);
>   - backoff resets only after 30 s of stable connection;
>   - decryption failures become `Error` events (sender id only);
>   - titles sync on `Contacts` and the first `QueueEmpty`.
> - **Core thread panic** → `Error` + `Offline`.
> - **Logging:** warn+ goes to the unified log (subsystem `dev.pitis.hush`), with sqlx statements off.
> - **Conversations:** `conversations()` lists only conversations with messages, so phone contacts don't flood the sidebar.
> - **Deferred to #16:** detecting "unlinked by the phone" (websocket handshake 401/403).

**Files:**
- Create: `core/sig-core/src/client.rs` (UniFFI surface), `core/sig-core/src/actor.rs` (presage thread), `core/sig-core/src/events.rs`, `core/sig-core/src/error.rs`
- Modify: `core/sig-core/src/lib.rs`

**Interfaces:**
- Consumes: `ProjectionDb::{open, apply, conversations, messages, search, reactions_for, message, upsert_conversation_title}`, `project`, `conversation_id` (Tasks 2–4).
- Produces (Swift-visible):
  - `#[uniffi::export(with_foreign)] trait EventListener: Send + Sync { fn on_event(&self, event: CoreEvent); }`
  - `CoreEvent` (`uniffi::Enum`): `LinkUrl { url: String }`, `Linked`, `Ready` (queue drained), `ConversationsChanged`, `MessageChanged { conversation_id: String, author: String, sent_at: i64 }`, `Typing { conversation_id: String, who: String, started: bool }`, `Connection { state: ConnectionState }`, `Error { message: String }`
  - `ConnectionState` (`uniffi::Enum`): `Connecting`, `Online`, `Offline`, `Unlinked`
  - `CoreError` (`uniffi::Error`, `thiserror`): `Storage { message: String }`, `Signal { message: String }`, `NotLinked`, `Closed`
  - `Client` (`uniffi::Object`):
    - `#[uniffi::constructor] fn open(data_dir: String, db_key_hex: String, listener: Arc<dyn EventListener>) -> Result<Arc<Client>, CoreError>`
    - `fn is_linked(&self) -> bool`
    - `fn link(&self, device_name: String)` (progress via events)
    - `fn start(&self)` (begin receive loop; emits Connection/Ready/MessageChanged)
    - `async fn conversations(&self) -> Result<Vec<Conversation>, CoreError>`
    - `async fn messages(&self, conversation_id: String, before: Option<MessageKey>, limit: u32) -> Result<Vec<Message>, CoreError>`
    - `async fn search(&self, query: String, limit: u32) -> Result<Vec<Message>, CoreError>`
    - `async fn reactions(&self, author: String, sent_at: i64) -> Result<Vec<Reaction>, CoreError>`

- [ ] **Step 1: Failing test** for the part that doesn't need Signal servers — `events::from_applied`, and `Client::open` on a fresh dir reporting `is_linked() == false`:

```rust
// core/sig-core/src/client.rs (tests)
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Sink(Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink { fn on_event(&self, e: CoreEvent) { self.0.lock().unwrap().push(e) } }

    #[test]
    fn fresh_dir_is_not_linked_and_queries_work() {
        let dir = std::env::temp_dir().join(format!("hush-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let key = "ab".repeat(32);
        let client = Client::open(dir.to_string_lossy().into(), key, sink.clone()).unwrap();
        assert!(!client.is_linked());
        let convs = futures::executor::block_on(client.conversations()).unwrap();
        assert!(convs.is_empty());
    }
}
```

and in `events.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::db::Applied;
    #[test]
    fn applied_maps_to_events() {
        assert!(from_applied(Applied::Nothing).is_none());
        let e = from_applied(Applied::MessageChanged { conversation_id: "c:a".into(), author: "a".into(), sent_at: 1 }).unwrap();
        assert!(matches!(e, CoreEvent::MessageChanged { sent_at: 1, .. }));
    }
}
```

- [ ] **Step 2: Run, expect FAIL.** Run: `cd core && cargo test -p sig-core client events`

- [ ] **Step 3: Implement `error.rs` + `events.rs`**

```rust
// error.rs
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("storage: {message}")] Storage { message: String },
    #[error("signal: {message}")] Signal { message: String },
    #[error("not linked")] NotLinked,
    #[error("core closed")] Closed,
}
impl From<sqlx::Error> for CoreError { fn from(e: sqlx::Error) -> Self { Self::Storage { message: e.to_string() } } }
```

```rust
// events.rs
use crate::projection::db::Applied;

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionState { Connecting, Online, Offline, Unlinked }

#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreEvent {
    LinkUrl { url: String },
    Linked,
    Ready,
    ConversationsChanged,
    MessageChanged { conversation_id: String, author: String, sent_at: i64 },
    Typing { conversation_id: String, who: String, started: bool },
    Connection { state: ConnectionState },
    Error { message: String },
}

#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

pub fn from_applied(a: Applied) -> Option<CoreEvent> {
    match a {
        Applied::Nothing => None,
        Applied::MessageChanged { conversation_id, author, sent_at } => Some(CoreEvent::MessageChanged { conversation_id, author, sent_at }),
        Applied::Typing { conversation_id, who, started } => Some(CoreEvent::Typing { conversation_id, who, started }),
    }
}
```

- [ ] **Step 4: Implement `actor.rs`** — one OS thread, current-thread runtime, `LocalSet`, commands over `tokio::sync::mpsc`:

```rust
use std::{path::PathBuf, sync::Arc, time::Duration};
use futures::{channel::oneshot as foneshot, StreamExt};
use presage::{libsignal_service::configuration::SignalServers, manager::Registered,
    model::{identity::OnNewIdentity, messages::Received}, store::{ContentsStore, Thread}, Manager};
use presage_store_sqlite::SqliteStore;
use tokio::sync::{mpsc, oneshot};
use crate::{events::*, projection::{project, conversation_id, ConversationKind, ProjectionDb}, error::CoreError};

pub enum Command {
    Link { device_name: String },
    Start,
}

pub struct Handles { pub tx: mpsc::UnboundedSender<Command>, pub projection: ProjectionDb, pub linked: bool }

pub fn spawn(data_dir: PathBuf, key: String, listener: Arc<dyn EventListener>) -> Result<Handles, CoreError> {
    std::fs::create_dir_all(&data_dir).map_err(|e| CoreError::Storage { message: e.to_string() })?;
    let (tx, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(ProjectionDb, bool), CoreError>>();
    std::thread::Builder::new().name("sig-core".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, run(data_dir, key, listener, rx, ready_tx));
    }).map_err(|e| CoreError::Storage { message: e.to_string() })?;
    let (projection, linked) = ready_rx.blocking_recv().map_err(|_| CoreError::Closed)??;
    Ok(Handles { tx, projection, linked })
}

async fn run(dir: PathBuf, key: String, listener: Arc<dyn EventListener>,
             mut rx: mpsc::UnboundedReceiver<Command>,
             ready: oneshot::Sender<Result<(ProjectionDb, bool), CoreError>>) {
    let presage_url = format!("sqlite://{}", dir.join("presage.db").display());
    let store = match SqliteStore::open_with_passphrase(&presage_url, Some(&key), OnNewIdentity::Trust).await {
        Ok(s) => s, Err(e) => { let _ = ready.send(Err(CoreError::Storage { message: e.to_string() })); return; }
    };
    let proj_url = format!("sqlite://{}", dir.join("projection.db").display());
    let projection = match ProjectionDb::open(&proj_url, Some(&key)).await {
        Ok(p) => p, Err(e) => { let _ = ready.send(Err(e.into())); return; }
    };
    let mut manager = Manager::load_registered(store.clone()).await.ok();
    let _ = ready.send(Ok((projection.clone(), manager.is_some())));

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Link { device_name } => {
                let (url_tx, url_rx) = foneshot::channel();
                let l = listener.clone();
                let (res, _) = futures::future::join(
                    Manager::link_secondary_device(store.clone(), SignalServers::Production, device_name, url_tx),
                    async move { if let Ok(url) = url_rx.await { l.on_event(CoreEvent::LinkUrl { url: url.to_string() }) } },
                ).await;
                match res {
                    Ok(mut m) => {
                        if let Err(e) = m.request_contacts().await { tracing::warn!(%e, "request_contacts failed"); }
                        manager = Some(m);
                        listener.on_event(CoreEvent::Linked);
                    }
                    Err(e) => listener.on_event(CoreEvent::Error { message: format!("linking failed: {e}") }),
                }
            }
            Command::Start => match manager.clone() {
                Some(m) => { tokio::task::spawn_local(receive_loop(m, projection.clone(), listener.clone())); }
                None => listener.on_event(CoreEvent::Connection { state: ConnectionState::Unlinked }),
            },
        }
    }
}

async fn receive_loop(mut m: Manager<SqliteStore, Registered>, db: ProjectionDb, l: Arc<dyn EventListener>) {
    let me = m.registration_data().service_ids.aci.to_string();
    let mut backoff = Duration::from_secs(1);
    loop {
        l.on_event(CoreEvent::Connection { state: ConnectionState::Connecting });
        let stream = match m.receive_messages().await {
            Ok(s) => s,
            Err(e) => {
                l.on_event(CoreEvent::Error { message: format!("connect: {e}") });
                l.on_event(CoreEvent::Connection { state: ConnectionState::Offline });
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        l.on_event(CoreEvent::Connection { state: ConnectionState::Online });
        futures::pin_mut!(stream);
        while let Some(received) = stream.next().await {
            match received {
                Received::QueueEmpty => { sync_titles(&m, &db).await; l.on_event(CoreEvent::Ready) }
                Received::Contacts => { sync_titles(&m, &db).await; l.on_event(CoreEvent::ConversationsChanged) }
                Received::DecryptionError(sid) => tracing::warn!(?sid, "decryption error"),
                Received::Content(content) => {
                    for op in project(&content, &me) {
                        match db.apply(&op).await {
                            Ok(a) => if let Some(e) = from_applied(a) { l.on_event(e) },
                            Err(e) => l.on_event(CoreEvent::Error { message: format!("projection: {e}") }),
                        }
                    }
                }
            }
        }
        l.on_event(CoreEvent::Connection { state: ConnectionState::Offline });
        tokio::time::sleep(backoff).await;
    }
}

/// Copy contact names and group titles from presage's store into the projection.
async fn sync_titles(m: &Manager<SqliteStore, Registered>, db: &ProjectionDb) {
    if let Ok(contacts) = m.store().contacts().await {
        for c in contacts.flatten() {
            let id = format!("c:{}", c.uuid);
            let _ = db.upsert_conversation_title(&id, &c.name).await;
        }
    }
    if let Ok(groups) = m.store().groups().await {
        for (key, g) in groups.flatten() {
            let id = conversation_id(&Thread::Group(key));
            let _ = db.upsert_conversation_title(&id, &g.title).await;
        }
    }
}
```

Notes for the implementer: `registration_data().service_ids.aci` field path and the `contacts()/groups()` iterator item types must match the pinned presage (check `presage/src/manager/registered.rs` `RegistrationData` and `store.rs` `ContentsStore`); adjust the access expressions, not the design. Message *content* is never logged (privacy) — only ids and errors.

- [ ] **Step 5: Implement `client.rs`**

```rust
use std::{path::PathBuf, sync::Arc};
use crate::{actor::{self, Command}, error::CoreError, events::EventListener, projection::*};

#[derive(uniffi::Object)]
pub struct Client {
    tx: tokio::sync::mpsc::UnboundedSender<Command>,
    db: ProjectionDb,
    linked: std::sync::atomic::AtomicBool,
}

#[uniffi::export]
impl Client {
    #[uniffi::constructor]
    pub fn open(data_dir: String, db_key_hex: String, listener: Arc<dyn EventListener>) -> Result<Arc<Self>, CoreError> {
        let h = actor::spawn(PathBuf::from(data_dir), db_key_hex, listener)?;
        Ok(Arc::new(Self { tx: h.tx, db: h.projection, linked: h.linked.into() }))
    }
    pub fn is_linked(&self) -> bool { self.linked.load(std::sync::atomic::Ordering::Relaxed) }
    pub fn link(&self, device_name: String) { let _ = self.tx.send(Command::Link { device_name }); }
    pub fn start(&self) { let _ = self.tx.send(Command::Start); }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    pub async fn conversations(&self) -> Result<Vec<Conversation>, CoreError> { Ok(self.db.conversations().await?) }
    pub async fn messages(&self, conversation_id: String, before: Option<MessageKey>, limit: u32) -> Result<Vec<Message>, CoreError> {
        Ok(self.db.messages(&conversation_id, before, limit).await?)
    }
    pub async fn search(&self, query: String, limit: u32) -> Result<Vec<Message>, CoreError> { Ok(self.db.search(&query, limit).await?) }
    pub async fn reactions(&self, author: String, sent_at: i64) -> Result<Vec<Reaction>, CoreError> { Ok(self.db.reactions_for(&author, sent_at).await?) }
}
```

`Client` needs `linked` set true on `Linked`: wrap listener in `actor::spawn` with a small adapter that flips an `Arc<AtomicBool>` shared with `Client` when it sees `CoreEvent::Linked`. (Store the `Arc<AtomicBool>` in `Client` instead of a bare `AtomicBool`.) The sqlx pool is runtime-agnostic for queries issued from UniFFI's tokio runtime; if sqlx complains about a missing runtime, add `tokio` feature `rt-multi-thread` and uniffi feature `tokio`.

Add `uniffi = { workspace = true, features = ["tokio"] }` in `sig-core/Cargo.toml`. In `lib.rs`: `mod actor; mod client; mod error; mod events; mod projection; pub use client::Client; pub use events::*; pub use error::CoreError; pub use projection::*;` and re-export `projector`/`ids` items through `projection/mod.rs` (`pub mod ids; pub mod projector; pub use ids::conversation_id; pub use projector::{project, Op};`).

- [ ] **Step 6: Run all core tests, expect PASS.** Run: `cd core && cargo test -p sig-core`

- [ ] **Step 7: Regenerate bindings to confirm UniFFI accepts the surface.** Run: `./scripts/build-core.sh debug && grep -c "class Client" app/Generated/sig_core.swift` → `1`.

- [ ] **Step 8: Commit** — `git add -A && git commit -m "feat(core): presage actor, UniFFI Client, link + receive loop with events"`

---

### Task 6: Spike UI — Keychain key, QR link screen, live event log (manual E2E with phone)

> **As built (PR #27, after review):**
> - **Signing:** with the owner's Apple Development identity (team `95KQ82C586`, `app/project.yml`). An ad-hoc signature changes every build, and the Keychain then blocks `dbKey()` on an access prompt. This was found when the first run hung.
> - **Window:** a single `Window` scene owns one `CoreBridge` at app level. A second window would hit the data-dir lock, and a window close would block on joining the core thread.
> - **CoreBridge:**
>   - takes an injectable opener (`any ClientProtocol`), and tests use `FakeClient`;
>   - `boot()` runs once, off the main thread;
>   - conversation refreshes are coalesced (one in flight; the newest result wins);
>   - only a linked core moves the UI to the chats;
>   - a fresh link code clears the old error, and "Try again" shows only when no code is showing.
> - **Keychain:** throws on any error other than "not found", and on a creation race returns the stored key.
> - **Tests:** the app skips booting under XCTest (`XCTestConfigurationFilePath`).
> - **Logging:** state goes to the unified log (category `app`); core error text is logged `.private`.
> - **Build:** `build-core.sh` sets `MACOSX_DEPLOYMENT_TARGET=15.0`.
> - **Probe:** `cargo run --example link_probe` prints core events while linking.

**Files:**
- Create: `app/Hush/Keychain.swift`, `app/Hush/CoreBridge.swift`, `app/Hush/LinkView.swift`, `app/Hush/QRCode.swift`
- Modify: `app/Hush/ContentView.swift`, `app/Hush/HushApp.swift`
- Test: `app/HushTests/KeychainTests.swift`

**Interfaces:**
- Consumes: Swift `Client.open(dataDir:dbKeyHex:listener:)`, `isLinked()`, `link(deviceName:)`, `start()`, `conversations()`, protocol `EventListener`, enum `CoreEvent` (generated by Task 5).
- Produces: `Keychain.dbKey() throws -> String` (creates once, returns same value after), `@MainActor @Observable final class CoreBridge { var linkURL: String?; var linked: Bool; var connection: ConnectionState; var log: [String]; var conversations: [Conversation] }` — Plan 2 grows this into `AppStore`.

- [ ] **Step 1: Failing Keychain test**

```swift
import XCTest
@testable import Hush

final class KeychainTests: XCTestCase {
    func testKeyIsStableAnd64Hex() throws {
        let k = Keychain(service: "dev.pitis.hush.tests", account: "db-key-\(UUID())")
        defer { k.delete() }
        let a = try k.dbKey(), b = try k.dbKey()
        XCTAssertEqual(a, b)
        XCTAssertEqual(a.count, 64)
        XCTAssertTrue(a.allSatisfy(\.isHexDigit))
    }
}
```

- [ ] **Step 2: Run, expect FAIL.** Run: `cd app && xcodegen && xcodebuild -project Hush.xcodeproj -scheme Hush -destination 'platform=macOS,arch=arm64' test 2>&1 | tail -5`

- [ ] **Step 3: Implement `Keychain.swift`**

```swift
import Foundation
import Security

struct Keychain {
    var service = "dev.pitis.hush"
    var account = "db-key"

    func dbKey() throws -> String {
        if let existing = read() { return existing }
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, 32, &bytes) == errSecSuccess else { throw KeychainError.random }
        let hex = bytes.map { String(format: "%02x", $0) }.joined()
        let add: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                  kSecAttrAccount as String: account, kSecValueData as String: Data(hex.utf8),
                                  kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly]
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else { throw KeychainError.status(status) }
        return hex
    }

    private func read() -> String? {
        let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                kSecAttrAccount as String: account, kSecReturnData as String: true]
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let d = out as? Data else { return nil }
        return String(data: d, encoding: .utf8)
    }

    func delete() {
        SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                       kSecAttrAccount as String: account] as CFDictionary)
    }
}

enum KeychainError: Error { case random, status(OSStatus) }
```

- [ ] **Step 4: Run, expect PASS.**

- [ ] **Step 5: `QRCode.swift`, `CoreBridge.swift`, `LinkView.swift`, `ContentView.swift`**

```swift
// QRCode.swift
import AppKit
import CoreImage.CIFilterBuiltins

enum QRCode {
    static func image(for string: String, scale: CGFloat = 8) -> NSImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(string.utf8)
        guard let out = f.outputImage?.transformed(by: .init(scaleX: scale, y: scale)) else { return nil }
        let rep = NSCIImageRep(ciImage: out)
        let img = NSImage(size: rep.size); img.addRepresentation(rep)
        return img
    }
}
```

```swift
// CoreBridge.swift
import Foundation
import Observation

@MainActor @Observable
final class CoreBridge {
    var linkURL: String?
    var linked = false
    var connection: ConnectionState = .offline
    var log: [String] = []
    var conversations: [Conversation] = []
    private(set) var client: Client?

    func boot() {
        do {
            let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("Hush", isDirectory: true)
            let client = try Client.open(dataDir: dir.path, dbKeyHex: try Keychain().dbKey(), listener: Listener(bridge: self))
            self.client = client
            linked = client.isLinked()
            if linked { client.start() } else { client.link(deviceName: "Hush") }
        } catch { log.append("boot failed: \(error)") }
    }

    func handle(_ e: CoreEvent) {
        switch e {
        case .linkUrl(let url): linkURL = url
        case .linked: linked = true; linkURL = nil; client?.start()
        case .connection(let s): connection = s
        case .ready, .conversationsChanged: Task { conversations = (try? await client?.conversations()) ?? [] }
        case .messageChanged(let conv, let author, let sentAt):
            log.append("msg \(conv) \(author.prefix(8)) @\(sentAt)")
            Task { conversations = (try? await client?.conversations()) ?? [] }
        case .typing(let conv, _, let started): log.append("typing \(conv) \(started)")
        case .error(let m): log.append("error: \(m)")
        }
    }
}

final class Listener: EventListener, @unchecked Sendable {
    weak var bridge: CoreBridge?
    init(bridge: CoreBridge) { self.bridge = bridge }
    func onEvent(event: CoreEvent) { Task { @MainActor [weak bridge] in bridge?.handle(event) } }
}
```

```swift
// LinkView.swift
import SwiftUI

struct LinkView: View {
    let url: String?
    var body: some View {
        VStack(spacing: 16) {
            Text("Link Hush").font(.title2.bold())
            Text("On your phone: Signal → Settings → Linked devices → Link new device")
                .foregroundStyle(.secondary)
            if let url, let img = QRCode.image(for: url) {
                Image(nsImage: img).interpolation(.none).resizable().frame(width: 280, height: 280)
            } else { ProgressView() }
        }.padding(40)
    }
}
```

```swift
// ContentView.swift
import SwiftUI

struct ContentView: View {
    @State private var core = CoreBridge()
    var body: some View {
        Group {
            if !core.linked { LinkView(url: core.linkURL) }
            else {
                HSplitView {
                    List(core.conversations, id: \.id) { c in
                        VStack(alignment: .leading) {
                            Text(c.title.isEmpty ? c.id : c.title).bold()
                            Text(c.lastMessagePreview).lineLimit(1).foregroundStyle(.secondary)
                        }.badge(Int(c.unreadCount))
                    }.frame(minWidth: 260)
                    List(core.log.reversed(), id: \.self) { Text($0).font(.caption.monospaced()) }
                }
            }
        }
        .frame(minWidth: 700, minHeight: 450)
        .navigationTitle("Hush — \(String(describing: core.connection))")
        .task { core.boot() }
    }
}
```

- [ ] **Step 6: Build + unit tests pass.** Run: `make check` → `TEST SUCCEEDED`.

- [ ] **Step 7: Manual E2E (requires Pitis + phone).** Run: `make run` (or run from Xcode).
  Checklist:
  1. QR appears within ~5 s.
  2. Scan with phone → phone shows "Hush" in Linked devices; window switches to the split view; title shows `online`.
  3. Contacts/groups appear with names within ~30 s (after `Contacts` event).
  4. Send a message to yourself/another device from someone → log line `msg c:… @…` appears, conversation jumps to top with preview + unread badge.
  5. Send from **your phone** to someone → appears in that person's conversation (outgoing).
  6. React/edit/delete from phone → `msg` log line for the target.
  7. Quit and relaunch → no QR, goes straight to online, previous conversations still listed.
  8. Activity Monitor: note Hush memory after 5 min idle (target < 100 MB).
  Record results in the issue for this task.

- [ ] **Step 8: Commit** — `git add -A && git commit -m "feat(app): QR linking, Keychain DB key and live event log spike"`

---

## Self-review notes
- Spec coverage for Plan 1: architecture (actor, projection DB, UniFFI, Keychain, data dir) ✓; link ✓; receive ✓; projection of text/reactions/edits/deletes/receipts/typing/quotes ✓; FTS ✓; connection states + backoff ✓; undecryptable skipped ✓. Sending, attachments, timeline UI, composer, notifications, ⌘K, Quick Look, voice notes, unlink handling, log view, packaging → Plan 2 (tracked as issues).
- Deviation from spec, deliberate: projection lives in `projection.db`, not presage's file (sqlx migration table collision). Same key.
