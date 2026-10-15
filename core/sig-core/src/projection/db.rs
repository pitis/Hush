use std::path::PathBuf;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{ConnectOptions, Row};

use super::model::*;
use super::projector::Op;

/// What applying an `Op` changed, for the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    MessageChanged {
        conversation_id: String,
        key: MessageKey,
    },
    Typing {
        conversation_id: String,
        who: String,
        started: bool,
    },
    AttachmentChanged {
        conversation_id: String,
        message: MessageKey,
        id: i64,
    },
    /// Something was added to a chat's timeline that isn't a message.
    ChatEvent {
        conversation_id: String,
    },
}

/// Encrypted SQLite store the UI reads from. Separate from presage's database because both use
/// `sqlx::migrate!`, which would collide on `_sqlx_migrations`.
///
/// The pool has one connection, so a method must never call another `ProjectionDb` method while
/// it holds a transaction: multi-step work goes through `&mut SqliteConnection` helpers.
///
/// It also owns the attachments folder (`with_files`): files are written and removed together
/// with their rows, so a message deleted for everyone leaves nothing behind.
#[derive(Clone)]
pub struct ProjectionDb {
    pool: SqlitePool,
    files: Option<PathBuf>,
}

/// Signal's limit for one attachment: larger ones are neither sent nor downloaded.
pub const MAX_ATTACHMENT: u64 = 100 * 1024 * 1024;

/// The chat list's preview when the latest thing in a chat is a message that couldn't be
/// decrypted.
pub const UNDECRYPTABLE_PREVIEW: &str = "A message couldn't be decrypted";

/// Images and voice notes up to this size download as they arrive; anything else on request.
pub const AUTO_DOWNLOAD_MAX: u64 = 25 * 1024 * 1024;

impl ProjectionDb {
    /// Opens (creating if needed) and migrates the database. `key` is the 64-char hex SQLCipher
    /// key; `None` opens an unencrypted database (tests).
    pub async fn open(url: &str, key: Option<&str>) -> Result<Self, sqlx::Error> {
        // Statement logging is off: the key pragma would otherwise be logged in clear at debug level.
        let mut opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .disable_statement_logging();
        if let Some(k) = key {
            if k.len() != 64 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(sqlx::Error::Configuration(
                    "database key must be 64 hex characters".into(),
                ));
            }
            opts = opts.pragma("key", format!("\"x'{k}'\""));
        }
        // One connection: in-memory databases are per connection, and the app's write volume is tiny.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, files: None })
    }

    /// Closes the database: later calls fail. Before its file is removed (unlinking).
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Keeps attachment files in `dir` (created when the first one is written).
    pub fn with_files(mut self, dir: PathBuf) -> Self {
        self.files = Some(dir);
        self
    }

    /// Creates the conversation or updates its title; an empty title never replaces a known one.
    pub async fn upsert_conversation_title(&self, id: &str, title: &str) -> Result<(), sqlx::Error> {
        let kind = kind_of(id)?;
        sqlx::query(
            "INSERT INTO conversations(id, kind, title) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET title = CASE WHEN ?3 = '' THEN title ELSE ?3 END",
        )
        .bind(id)
        .bind(kind_to_i64(kind))
        .bind(title)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Inserts a message and refreshes its conversation's summary. Returns false (and changes
    /// nothing) if a message with the same `(author, sent_at)` already exists. A deleted message
    /// is stored without its text.
    pub async fn insert_message(&self, m: &Message) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let inserted = insert_message_in(&mut tx, m).await?;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Our own message with files: the message, and a copy of each file in the attachments
    /// folder (Ready to upload), all or nothing. False (and nothing stored) if a message with the
    /// same key exists.
    pub async fn insert_outgoing(&self, m: &Message, files: &[OutgoingFile]) -> Result<bool, sqlx::Error> {
        let dir = self
            .files
            .clone()
            .ok_or_else(|| sqlx::Error::Configuration("no attachments folder".into()))?;
        // Copies can be slow (large files, files still in iCloud): made before the transaction,
        // off the async threads, so the one database connection isn't held meanwhile.
        let sources: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let (copy_dir, sent_at) = (dir.clone(), m.sent_at);
        let staged = tokio::task::spawn_blocking(move || stage_copies(&copy_dir, sent_at, &sources))
            .await
            .map_err(|e| sqlx::Error::Io(std::io::Error::other(e)))??;
        let mut names: Vec<String> = staged.iter().map(|(name, _)| name.clone()).collect();
        let mut tx = self.pool.begin().await?;
        let result = async {
            if !insert_message_in(&mut tx, m).await? {
                return Ok(false);
            }
            for (i, f) in files.iter().enumerate() {
                let id = add_outgoing_file(&mut tx, m, f, i as i64, staged[i].1).await?;
                let file = id.to_string();
                std::fs::rename(dir.join(&names[i]), dir.join(&file))?;
                names[i] = file.clone();
                sqlx::query("UPDATE attachments SET file = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(&file)
                    .execute(&mut *tx)
                    .await?;
            }
            Ok::<_, sqlx::Error>(true)
        }
        .await;
        match result {
            Ok(true) => {
                tx.commit().await?;
                Ok(true)
            }
            other => {
                drop(tx); // rolled back: the message isn't stored either
                self.remove_files(&names);
                other
            }
        }
    }

    /// Conversations that have messages, most recent first. Contacts known only by name (synced
    /// from the phone) aren't listed until a message arrives.
    pub async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT c.*, m.author AS last_author, m.is_outgoing AS last_outgoing, m.status AS last_status,
               m.deleted AS last_deleted, a.content_type AS file_type, a.file_name AS file_name,
               a.is_voice_note AS file_voice, m.sent_at AS last_sent_at, m.kind AS last_kind,
               m.kind_text AS last_kind_text,
               (SELECT MAX(at) FROM chat_events e WHERE e.conversation_id = c.id AND e.kind = 0) AS failed_at
             FROM conversations c LEFT JOIN messages m ON m.id = (SELECT id FROM messages
               WHERE conversation_id = c.id ORDER BY sent_at DESC, author DESC LIMIT 1)
             LEFT JOIN attachments a ON a.message_author = m.author AND a.message_sent_at = m.sent_at
               AND a.position = 0
             WHERE c.last_message_at > 0 ORDER BY c.last_message_at DESC, c.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_conversation).collect()
    }

    /// Up to `limit` messages of a conversation that come before `before` (or the newest ones),
    /// oldest first. Messages are ordered by `(sent_at, author)`, so equal timestamps page correctly.
    pub async fn messages(
        &self,
        conversation_id: &str,
        before: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let mut rows = match before {
            Some(k) => {
                sqlx::query(
                    "SELECT * FROM messages WHERE conversation_id = ?1 AND (sent_at, author) < (?2, ?3)
                     ORDER BY sent_at DESC, author DESC LIMIT ?4",
                )
                .bind(conversation_id)
                .bind(k.sent_at)
                .bind(k.author)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT * FROM messages WHERE conversation_id = ?1 ORDER BY sent_at DESC, author DESC LIMIT ?2",
                )
                .bind(conversation_id)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.reverse();
        rows.iter().map(row_to_message).collect()
    }

    /// Search over message bodies, newest first, optionally in one conversation. All words must
    /// match, the last one as a prefix ("see tomo"), with case and diacritics folded. Queries of
    /// three or more characters also match inside words and in text without spaces (Chinese,
    /// Japanese…); shorter ones outside ASCII ("明天", "🎉") are looked for as plain text. FTS5
    /// operators in the input are plain text.
    pub async fn search(
        &self,
        query: &str,
        conversation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let query = query.trim();
        let mut hits = vec![];
        if let Some(words) = word_query(query) {
            hits.extend(self.fts_search(Index::Words, &words, conversation_id, limit).await?);
        }
        if query.chars().count() >= 3 {
            let phrase = format!("\"{}\"", query.replace('"', "\"\""));
            hits.extend(
                self.fts_search(Index::Trigrams, &phrase, conversation_id, limit)
                    .await?,
            );
        } else if !query.is_empty() && (!query.is_ascii() || word_query(query).is_none()) {
            hits.extend(self.text_search(query, conversation_id, limit).await?);
        }
        // newest first, each message once
        hits.sort_by(|a, b| (b.message.sent_at, &b.message.author).cmp(&(a.message.sent_at, &a.message.author)));
        hits.dedup_by(|a, b| a.message.key() == b.message.key());
        hits.truncate(limit as usize);
        Ok(hits)
    }

    async fn fts_search(
        &self,
        index: Index,
        matching: &str,
        conversation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        // The newest by arrival (rowid), which FTS5 walks without sorting every match; `search`
        // orders these by time. The highlight markers are U+E000 and U+E001.
        let sql = match index {
            Index::Words => {
                "SELECT m.*, snippet(messages_fts, 0, char(57344), char(57345), '…', 12) AS snippet
                 FROM messages_fts f JOIN messages m ON m.id = f.rowid
                 WHERE messages_fts MATCH ?1 AND m.deleted = 0 AND (?2 IS NULL OR m.conversation_id = ?2)
                 ORDER BY f.rowid DESC LIMIT ?3"
            }
            Index::Trigrams => {
                "SELECT m.*, snippet(messages_trigram, 0, char(57344), char(57345), '…', 12) AS snippet
                 FROM messages_trigram f JOIN messages m ON m.id = f.rowid
                 WHERE messages_trigram MATCH ?1 AND m.deleted = 0 AND (?2 IS NULL OR m.conversation_id = ?2)
                 ORDER BY f.rowid DESC LIMIT ?3"
            }
        };
        let rows = sqlx::query(sql)
            .bind(matching)
            .bind(conversation_id)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok(SearchHit {
                    message: row_to_message(r)?,
                    snippet: r.try_get("snippet")?,
                })
            })
            .collect()
    }

    /// Plain-text search, for short queries no index can find.
    async fn text_search(
        &self,
        needle: &str,
        conversation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let pattern = format!(
            "%{}%",
            needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
        );
        let rows = sqlx::query(
            "SELECT * FROM messages WHERE body LIKE ?1 ESCAPE '\\' AND deleted = 0
               AND (?2 IS NULL OR conversation_id = ?2)
             ORDER BY sent_at DESC, author DESC LIMIT ?3",
        )
        .bind(pattern)
        .bind(conversation_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let message = row_to_message(r)?;
                let snippet = text_snippet(message.body.as_deref().unwrap_or_default(), needle);
                Ok(SearchHit { message, snippet })
            })
            .collect()
    }

    /// Records something that happened in a chat (see `ChatEvent`).
    pub async fn add_chat_event(
        &self,
        conversation_id: &str,
        at: i64,
        kind: ChatEventKind,
        author: &str,
    ) -> Result<(), sqlx::Error> {
        let conv_kind = kind_of(conversation_id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(conversation_id)
            .bind(kind_to_i64(conv_kind))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO chat_events(conversation_id, at, kind, author) VALUES (?1, ?2, ?3, ?4)")
            .bind(conversation_id)
            .bind(at)
            .bind(kind.to_i64())
            .bind(author)
            .execute(&mut *tx)
            .await?;
        refresh_summary(&mut tx, conversation_id).await?;
        tx.commit().await
    }

    /// A chat's events from `since` (ms) on, oldest first.
    pub async fn chat_events(&self, conversation_id: &str, since: i64) -> Result<Vec<ChatEvent>, sqlx::Error> {
        let rows: Vec<(i64, String, i64, i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT id, conversation_id, at, kind, author, value FROM chat_events
             WHERE conversation_id = ?1 AND at >= ?2 ORDER BY at, id",
        )
        .bind(conversation_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, conversation_id, at, kind, author, value)| {
                Ok(ChatEvent {
                    id,
                    conversation_id,
                    at,
                    kind: ChatEventKind::from_i64(kind).ok_or_else(|| decode_err(format!("chat event kind {kind}")))?,
                    author,
                    value,
                })
            })
            .collect()
    }

    /// How many messages of a conversation are newer than `key` (to load enough of it to show
    /// that message).
    pub async fn count_newer(&self, conversation_id: &str, key: &MessageKey) -> Result<u32, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND (sent_at, author) > (?2, ?3)",
        )
        .bind(conversation_id)
        .bind(key.sent_at)
        .bind(&key.author)
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Applies one projection op atomically. Targets may name an edit revision. Deletes and read
    /// state for messages not stored yet are remembered and applied when the message arrives;
    /// reactions are stored regardless; other ops on unknown messages change nothing.
    pub async fn apply(&self, op: &Op) -> Result<Vec<Applied>, sqlx::Error> {
        let changed = |m: Option<(String, MessageKey)>| -> Vec<Applied> {
            m.map(|(conversation_id, key)| Applied::MessageChanged { conversation_id, key })
                .into_iter()
                .collect()
        };
        match op {
            Op::InsertMessage(m) => Ok(if self.insert_message(m).await? {
                vec![Applied::MessageChanged {
                    conversation_id: m.conversation_id.clone(),
                    key: m.key(),
                }]
            } else {
                vec![]
            }),
            Op::Typing {
                conversation_id,
                who,
                started,
            } => Ok(vec![Applied::Typing {
                conversation_id: conversation_id.clone(),
                who: who.clone(),
                started: *started,
            }]),
            Op::React { target, reactor, emoji } => {
                let mut tx = self.pool.begin().await?;
                let target = &resolve(&mut tx, target).await?;
                if is_deleted(&mut tx, target).await? {
                    return Ok(vec![]);
                }
                let res = match emoji {
                    Some(e) => sqlx::query(
                        "INSERT INTO reactions(target_author, target_sent_at, reactor, emoji) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT DO UPDATE SET emoji = excluded.emoji WHERE emoji != excluded.emoji",
                    )
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .bind(reactor)
                    .bind(e),
                    None => sqlx::query(
                        "DELETE FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2 AND reactor = ?3",
                    )
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .bind(reactor),
                }
                .execute(&mut *tx)
                .await?;
                let m = if res.rows_affected() > 0 {
                    conversation_of(&mut tx, target).await?
                } else {
                    None
                };
                tx.commit().await?;
                Ok(changed(m))
            }
            Op::Edit {
                target,
                new_body,
                edited_at,
            } => {
                let mut tx = self.pool.begin().await?;
                let target = &resolve(&mut tx, target).await?;
                let res = sqlx::query(
                    "UPDATE messages SET body = ?3, edited = 1, edited_at = ?4
                     WHERE author = ?1 AND sent_at = ?2 AND deleted = 0 AND edited_at < ?4",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(new_body)
                .bind(edited_at)
                .execute(&mut *tx)
                .await?;
                if res.rows_affected() == 1 {
                    // later ops may target the message by this revision's timestamp
                    sqlx::query("INSERT OR IGNORE INTO message_revisions VALUES (?1, ?2, ?3)")
                        .bind(&target.author)
                        .bind(edited_at)
                        .bind(target.sent_at)
                        .execute(&mut *tx)
                        .await?;
                }
                let m = finish_update(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                Ok(changed(m))
            }
            Op::Delete { target } => {
                let mut tx = self.pool.begin().await?;
                let target = &resolve(&mut tx, target).await?;
                if conversation_of(&mut tx, target).await?.is_none() {
                    // the delete arrived first: remember it so the message is never shown
                    sqlx::query("INSERT OR IGNORE INTO pending_deletes VALUES (?1, ?2)")
                        .bind(&target.author)
                        .bind(target.sent_at)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    return Ok(vec![]);
                }
                let res = sqlx::query(
                    "UPDATE messages SET body = NULL, quote_text = NULL, deleted = 1
                     WHERE author = ?1 AND sent_at = ?2 AND deleted = 0",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .execute(&mut *tx)
                .await?;
                let files = forget_deleted_text(&mut tx, target).await?;
                let m = finish_update(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                self.remove_files(&files);
                Ok(changed(m))
            }
            Op::AddAttachments {
                message,
                attachments,
                preview,
            } => {
                let mut tx = self.pool.begin().await?;
                let Some((conversation_id, _)) = conversation_of(&mut tx, message).await? else {
                    return Ok(vec![]);
                };
                if is_deleted(&mut tx, message).await? {
                    return Ok(vec![]);
                }
                let image = preview.as_ref().and_then(|p| p.image.as_ref());
                let all = attachments.iter().zip(0i64..).chain(image.map(|a| (a, -1)));
                let mut out = vec![];
                for (a, position) in all {
                    let id: Option<i64> = sqlx::query_scalar(
                        "INSERT OR IGNORE INTO attachments(message_author, message_sent_at, position, content_type,
                           file_name, size, width, height, is_voice_note, pointer, state)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) RETURNING id",
                    )
                    .bind(&message.author)
                    .bind(message.sent_at)
                    .bind(position)
                    .bind(&a.content_type)
                    .bind(&a.file_name)
                    .bind(a.size as i64)
                    .bind(a.width)
                    .bind(a.height)
                    .bind(a.is_voice_note)
                    .bind(&a.pointer)
                    .bind(AttachmentState::Pending.to_i64())
                    .fetch_optional(&mut *tx)
                    .await?;
                    out.extend(id.map(|id| Applied::AttachmentChanged {
                        conversation_id: conversation_id.clone(),
                        message: message.clone(),
                        id,
                    }));
                }
                if let Some(p) = preview {
                    sqlx::query("INSERT OR IGNORE INTO link_previews VALUES (?1, ?2, ?3, ?4, ?5)")
                        .bind(&message.author)
                        .bind(message.sent_at)
                        .bind(&p.url)
                        .bind(&p.title)
                        .bind(&p.description)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(out)
            }
            Op::Settings {
                read_receipts,
                typing_indicators,
            } => {
                let mut tx = self.pool.begin().await?;
                for (key, value) in [
                    ("read_receipts", read_receipts),
                    ("typing_indicators", typing_indicators),
                ] {
                    if let Some(v) = value {
                        sqlx::query(
                            "INSERT INTO settings VALUES (?1, ?2) ON CONFLICT DO UPDATE SET value = excluded.value",
                        )
                        .bind(key)
                        .bind(v)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                tx.commit().await?;
                Ok(vec![])
            }
            Op::SetStatus { targets, status } => {
                let mut tx = self.pool.begin().await?;
                let mut out = vec![];
                for target in targets {
                    let target = &resolve(&mut tx, target).await?;
                    if conversation_of(&mut tx, target).await?.is_none() {
                        // e.g. read on the phone before the message reached us
                        sqlx::query(
                            "INSERT INTO pending_status VALUES (?1, ?2, ?3)
                             ON CONFLICT DO UPDATE SET status = MAX(status, excluded.status)",
                        )
                        .bind(&target.author)
                        .bind(target.sent_at)
                        .bind(status.to_i64())
                        .execute(&mut *tx)
                        .await?;
                        continue;
                    }
                    let res = sqlx::query(
                        "UPDATE messages SET status = ?3 WHERE author = ?1 AND sent_at = ?2 AND status < ?3",
                    )
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .bind(status.to_i64())
                    .execute(&mut *tx)
                    .await?;
                    if *status == MessageStatus::Read {
                        start_timer(&mut tx, target).await?;
                    }
                    out.extend(changed(finish_update(&mut tx, target, res.rows_affected()).await?));
                }
                tx.commit().await?;
                Ok(out)
            }
            Op::SetTimer {
                conversation_id,
                author,
                seconds,
                at,
            } => {
                let kind = kind_of(conversation_id)?;
                let mut tx = self.pool.begin().await?;
                sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
                    .bind(conversation_id)
                    .bind(kind_to_i64(kind))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE conversations SET expire_timer = ?2, timer_at = ?3 WHERE id = ?1 AND timer_at <= ?3",
                )
                .bind(conversation_id)
                .bind(i64::from(*seconds))
                .bind(at)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO chat_events(conversation_id, at, kind, author, value) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .bind(conversation_id)
                .bind(at)
                .bind(ChatEventKind::TimerChanged.to_i64())
                .bind(author)
                .bind(i64::from(*seconds))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(vec![Applied::ChatEvent {
                    conversation_id: conversation_id.clone(),
                }])
            }
        }
    }

    /// The chat's disappearing-message timer (seconds, 0: off), for our own new messages.
    pub async fn expire_timer(&self, conversation_id: &str) -> Result<u32, sqlx::Error> {
        let t: Option<i64> = sqlx::query_scalar("SELECT expire_timer FROM conversations WHERE id = ?1")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(t.and_then(|t| u32::try_from(t).ok()).unwrap_or(0))
    }

    /// Removes the messages whose timer ran out by `now_ms`: rows, search index, reactions,
    /// previews and files. Returns them.
    pub async fn delete_expired(&self, now_ms: i64) -> Result<Vec<(String, MessageKey)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let expired: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT conversation_id, author, sent_at FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1",
        )
        .bind(now_ms)
        .fetch_all(&mut *tx)
        .await?;
        let mut files = vec![];
        let mut out = vec![];
        for (conversation_id, author, sent_at) in expired {
            let key = MessageKey { author, sent_at };
            files.extend(forget_deleted_text(&mut tx, &key).await?);
            sqlx::query("DELETE FROM messages WHERE author = ?1 AND sent_at = ?2")
                .bind(&key.author)
                .bind(key.sent_at)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM message_revisions WHERE author = ?1 AND original_sent_at = ?2")
                .bind(&key.author)
                .bind(key.sent_at)
                .execute(&mut *tx)
                .await?;
            out.push((conversation_id, key));
        }
        let mut conversations: Vec<&String> = out.iter().map(|(c, _)| c).collect();
        conversations.dedup();
        for c in conversations {
            refresh_summary(&mut tx, c).await?;
        }
        tx.commit().await?;
        self.remove_files(&files);
        Ok(out)
    }

    /// Pending → Failed (a send that didn't go out). No other transition.
    pub async fn set_failed(&self, key: &MessageKey) -> Result<(), sqlx::Error> {
        self.transition(key, MessageStatus::Pending, MessageStatus::Failed)
            .await
    }

    /// Marks every outgoing Pending message Failed (e.g. at startup: its queued send died with the
    /// previous run). Failed rather than re-sent, because the send may already have gone out; a
    /// later receipt still raises it. Returns the affected keys.
    pub async fn fail_all_pending(&self) -> Result<Vec<(String, MessageKey)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "UPDATE messages SET status = ?1 WHERE is_outgoing = 1 AND status = ?2
             RETURNING conversation_id, author, sent_at",
        )
        .bind(MessageStatus::Failed.to_i64())
        .bind(MessageStatus::Pending.to_i64())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(c, author, sent_at)| (c, MessageKey { author, sent_at }))
            .collect())
    }

    /// Failed → Pending (retry). No other transition.
    pub async fn set_pending(&self, key: &MessageKey) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query("UPDATE messages SET status = ?3 WHERE author = ?1 AND sent_at = ?2 AND status = ?4")
            .bind(&key.author)
            .bind(key.sent_at)
            .bind(MessageStatus::Pending.to_i64())
            .bind(MessageStatus::Failed.to_i64())
            .execute(&mut *tx)
            .await?;
        finish_update(&mut tx, key, res.rows_affected()).await?;
        tx.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    async fn transition(&self, key: &MessageKey, from: MessageStatus, to: MessageStatus) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query("UPDATE messages SET status = ?3 WHERE author = ?1 AND sent_at = ?2 AND status = ?4")
            .bind(&key.author)
            .bind(key.sent_at)
            .bind(to.to_i64())
            .bind(from.to_i64())
            .execute(&mut *tx)
            .await?;
        finish_update(&mut tx, key, res.rows_affected()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The stored title of any conversation (also contacts not shown in the list), e.g. to name
    /// group senders.
    pub async fn title(&self, id: &str) -> Result<Option<String>, sqlx::Error> {
        let t: Option<String> = sqlx::query_scalar("SELECT title FROM conversations WHERE id = ?1 AND title != ''")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(t)
    }

    /// The incoming messages of a conversation that count as unread (as in `unread_count`), oldest
    /// first.
    pub async fn unread(&self, conversation_id: &str) -> Result<Vec<MessageKey>, sqlx::Error> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT author, sent_at FROM messages WHERE conversation_id = ?1
               AND is_outgoing = 0 AND deleted = 0 AND status < ?2 ORDER BY sent_at, author",
        )
        .bind(conversation_id)
        .bind(MessageStatus::Read.to_i64())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(author, sent_at)| MessageKey { author, sent_at })
            .collect())
    }

    pub async fn settings(&self) -> Result<Settings, sqlx::Error> {
        let rows: Vec<(String, bool)> = sqlx::query_as("SELECT key, value FROM settings")
            .fetch_all(&self.pool)
            .await?;
        let get = |k: &str| rows.iter().find(|(key, _)| key == k).map(|(_, v)| *v);
        Ok(Settings {
            read_receipts: get("read_receipts"),
            typing_indicators: get("typing_indicators"),
        })
    }

    /// Remembers that read receipts for `targets` still have to go out.
    pub async fn queue_receipts(&self, targets: &[MessageKey]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            sqlx::query("INSERT OR IGNORE INTO unsent_receipts VALUES (?1, ?2)")
                .bind(&t.author)
                .bind(t.sent_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Read receipts still to send, by sender then time.
    pub async fn unsent_receipts(&self) -> Result<Vec<MessageKey>, sqlx::Error> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT author, sent_at FROM unsent_receipts ORDER BY author, sent_at")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(author, sent_at)| MessageKey { author, sent_at })
            .collect())
    }

    pub async fn receipts_sent(&self, targets: &[MessageKey]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            sqlx::query("DELETE FROM unsent_receipts WHERE author = ?1 AND sent_at = ?2")
                .bind(&t.author)
                .bind(t.sent_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// A message's files, in order (not its link preview's image).
    pub async fn attachments(&self, key: &MessageKey) -> Result<Vec<Attachment>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT * FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 AND position >= 0
             ORDER BY position",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| self.row_to_attachment(r)).collect()
    }

    pub async fn attachment(&self, id: i64) -> Result<Option<Attachment>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM attachments WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(|r| self.row_to_attachment(r)).transpose()
    }

    pub async fn link_preview(&self, key: &MessageKey) -> Result<Option<LinkPreview>, sqlx::Error> {
        let Some(row) = sqlx::query("SELECT * FROM link_previews WHERE message_author = ?1 AND message_sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let image = sqlx::query(
            "SELECT * FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 AND position = -1",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_optional(&self.pool)
        .await?;
        Ok(Some(LinkPreview {
            url: row.try_get("url")?,
            title: row.try_get("title")?,
            description: row.try_get("description")?,
            image: image.as_ref().map(|r| self.row_to_attachment(r)).transpose()?,
        }))
    }

    /// Not yet downloaded images and voice notes up to `AUTO_DOWNLOAD_MAX`, oldest first.
    pub async fn auto_downloads(&self) -> Result<Vec<i64>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT id FROM attachments WHERE state = ?1 AND pointer IS NOT NULL AND size <= ?2
               AND (content_type LIKE 'image/%' OR is_voice_note = 1) ORDER BY id",
        )
        .bind(AttachmentState::Pending.to_i64())
        .bind(AUTO_DOWNLOAD_MAX as i64)
        .fetch_all(&self.pool)
        .await
    }

    /// Pending (or Failed, tried again) → Downloading. Returns the attachment's conversation,
    /// message and encoded pointer, or None if there is nothing to download. Files declared
    /// larger than `MAX_ATTACHMENT` (or of unknown size) are never fetched: the whole download
    /// is held in memory.
    pub async fn start_download(&self, id: i64) -> Result<Option<(String, MessageKey, Vec<u8>)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, i64, Vec<u8>)> = sqlx::query_as(
            "UPDATE attachments SET state = ?2 WHERE id = ?1 AND state IN (?3, ?4) AND pointer IS NOT NULL
               AND size BETWEEN 1 AND ?5
             RETURNING message_author, message_sent_at, pointer",
        )
        .bind(id)
        .bind(AttachmentState::Downloading.to_i64())
        .bind(AttachmentState::Pending.to_i64())
        .bind(AttachmentState::Failed.to_i64())
        .bind(MAX_ATTACHMENT as i64)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((author, sent_at, pointer)) = row else {
            return Ok(None);
        };
        let key = MessageKey { author, sent_at };
        let conversation = conversation_of(&mut tx, &key).await?;
        tx.commit().await?;
        Ok(conversation.map(|(c, key)| (c, key, pointer)))
    }

    /// Stores a finished download: its file and Ready, or Failed. Returns the attachment's
    /// conversation and message, if it still exists.
    pub async fn finish_download(
        &self,
        id: i64,
        data: Result<Vec<u8>, String>,
    ) -> Result<Option<(String, MessageKey)>, sqlx::Error> {
        let file = id.to_string();
        // the file is named by the id, which is never reused (AUTOINCREMENT): it is this
        // attachment's own, even if its message was deleted meanwhile
        let written = match (data, self.files.clone()) {
            (Ok(bytes), Some(dir)) => {
                let path = dir.join(&file);
                let wrote = tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(path, bytes))
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
                if let Err(e) = &wrote {
                    tracing::warn!(id, error = %e, "storing a downloaded attachment failed");
                }
                wrote
            }
            (Ok(_), None) => Err("no attachments folder".to_string()),
            (Err(e), _) => Err(e),
        };
        let (state, file) = match &written {
            Ok(()) => (AttachmentState::Ready, Some(file)),
            Err(_) => (AttachmentState::Failed, None),
        };
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, i64)> = sqlx::query_as(
            "UPDATE attachments SET state = ?2, file = ?3 WHERE id = ?1 AND state = ?4
             RETURNING message_author, message_sent_at",
        )
        .bind(id)
        .bind(state.to_i64())
        .bind(&file)
        .bind(AttachmentState::Downloading.to_i64())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((author, sent_at)) = row else {
            // the message was deleted meanwhile: its file must not stay
            tx.commit().await?;
            self.remove_files(&file.into_iter().collect::<Vec<_>>());
            return Ok(None);
        };
        let conversation = conversation_of(&mut tx, &MessageKey { author, sent_at }).await?;
        tx.commit().await?;
        Ok(conversation)
    }

    /// Downloads cut off by a quit start over.
    pub async fn reset_downloads(&self) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE attachments SET state = ?1 WHERE state = ?2")
            .bind(AttachmentState::Pending.to_i64())
            .bind(AttachmentState::Downloading.to_i64())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn remove_files(&self, files: &[String]) {
        if let Some(dir) = &self.files {
            for f in files {
                match std::fs::remove_file(dir.join(f)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        tracing::warn!(file = %f, error = %e, "removing an attachment file failed")
                    }
                    _ => {}
                }
            }
        }
    }

    fn row_to_attachment(&self, r: &SqliteRow) -> Result<Attachment, sqlx::Error> {
        let state: i64 = r.try_get("state")?;
        let size: i64 = r.try_get("size")?;
        let file: Option<String> = r.try_get("file")?;
        Ok(Attachment {
            id: r.try_get("id")?,
            message: MessageKey {
                author: r.try_get("message_author")?,
                sent_at: r.try_get("message_sent_at")?,
            },
            content_type: r.try_get("content_type")?,
            file_name: r.try_get("file_name")?,
            size: u64::try_from(size).map_err(|_| decode_err(format!("attachment size {size}")))?,
            width: r.try_get("width")?,
            height: r.try_get("height")?,
            is_voice_note: r.try_get("is_voice_note")?,
            path: file
                .zip(self.files.as_ref())
                .map(|(f, dir)| dir.join(f).to_string_lossy().into_owned()),
            state: AttachmentState::from_i64(state).ok_or_else(|| decode_err(format!("attachment state {state}")))?,
        })
    }

    pub async fn message(&self, key: &MessageKey) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM messages WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(row_to_message).transpose()
    }

    /// Reactions on a message, one per reactor, ordered by reactor.
    pub async fn reactions(&self, key: &MessageKey) -> Result<Vec<Reaction>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT reactor, emoji FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2 ORDER BY reactor",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Reaction {
                    reactor: r.try_get("reactor")?,
                    emoji: r.try_get("emoji")?,
                })
            })
            .collect()
    }
}

/// After an UPDATE of one message: refresh its conversation's summary if a row changed.
async fn finish_update(
    conn: &mut SqliteConnection,
    target: &MessageKey,
    rows: u64,
) -> Result<Option<(String, MessageKey)>, sqlx::Error> {
    if rows == 0 {
        return Ok(None);
    }
    let m = conversation_of(conn, target).await?;
    if let Some((conv, _)) = &m {
        refresh_summary(conn, conv).await?;
    }
    Ok(m)
}

/// Maps a key that may name an edit revision to the original message's key.
async fn resolve(conn: &mut SqliteConnection, key: &MessageKey) -> Result<MessageKey, sqlx::Error> {
    let original: Option<i64> =
        sqlx::query_scalar("SELECT original_sent_at FROM message_revisions WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(conn)
            .await?;
    Ok(MessageKey {
        author: key.author.clone(),
        sent_at: original.unwrap_or(key.sent_at),
    })
}

async fn is_deleted(conn: &mut SqliteConnection, key: &MessageKey) -> Result<bool, sqlx::Error> {
    let deleted: Option<bool> = sqlx::query_scalar("SELECT deleted FROM messages WHERE author = ?1 AND sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_optional(conn)
        .await?;
    Ok(deleted == Some(true))
}

/// Drops what other rows still hold of a deleted message: its reactions and quotes of its text.
/// Returns the attachment files to remove once the transaction is committed.
async fn forget_deleted_text(conn: &mut SqliteConnection, key: &MessageKey) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query("DELETE FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE messages SET quote_text = NULL WHERE quote_author = ?1 AND quote_sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM link_previews WHERE message_author = ?1 AND message_sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    let files: Vec<Option<String>> =
        sqlx::query_scalar("DELETE FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 RETURNING file")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_all(&mut *conn)
            .await?;
    Ok(files.into_iter().flatten().collect())
}

/// The two full-text indexes over message bodies.
enum Index {
    /// Whole words and prefixes, case and diacritics folded (unicode61).
    Words,
    /// Any three characters in a row (trigram).
    Trigrams,
}

/// The words of a query for the unicode61 index: each quoted (so FTS5 syntax is plain text), all
/// required, the last one a prefix. None if a part has no letters or digits (an emoji, ":)"):
/// that index can't see it, so its matches would ignore that part.
fn word_query(query: &str) -> Option<String> {
    let parts: Vec<&str> = query.split_whitespace().collect();
    if parts.iter().any(|w| !w.chars().any(char::is_alphanumeric)) {
        return None;
    }
    let words: Vec<String> = parts
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    let (last, rest) = words.split_last()?;
    Some(
        rest.iter()
            .chain([&format!("{last}*")])
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Up to about 40 characters around the first match of `needle`, the match marked. Case is
/// ignored for ASCII only, as SQLite's LIKE does: ASCII lowercasing keeps every byte where it
/// was, so positions found in the lowercased text are always character boundaries of the body
/// (full Unicode lowercasing can change lengths and cut a character in half).
fn text_snippet(body: &str, needle: &str) -> String {
    let Some(at) = body.to_ascii_lowercase().find(&needle.to_ascii_lowercase()) else {
        return body.chars().take(40).collect();
    };
    let end = at + needle.len();
    let before: String = body[..at]
        .chars()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = body[end..].chars().take(20).collect();
    let dots = |cut: bool| if cut { "…" } else { "" };
    format!(
        "{}{before}{HIGHLIGHT_START}{}{HIGHLIGHT_END}{after}{}",
        dots(before.len() < at),
        &body[at..end],
        dots(after.len() < body.len() - end)
    )
}

/// Inserts a message and refreshes its conversation's summary (see `insert_message`).
async fn insert_message_in(conn: &mut SqliteConnection, m: &Message) -> Result<bool, sqlx::Error> {
    let kind = kind_of(&m.conversation_id)?;
    sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
        .bind(&m.conversation_id)
        .bind(kind_to_i64(kind))
        .execute(&mut *conn)
        .await?;
    let quote = m.quote.as_ref();
    let (body, quote_text) = if m.deleted {
        (None, None)
    } else {
        (m.body.as_ref(), quote.and_then(|q| q.text.as_ref()))
    };
    let (kind, kind_text) = m.kind.to_row();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
           status, edited, deleted, quote_author, quote_sent_at, quote_text, kind, kind_text, expires_in,
           expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )
    .bind(&m.conversation_id)
    .bind(&m.author)
    .bind(m.sent_at)
    .bind(m.received_at)
    .bind(body)
    .bind(m.is_outgoing)
    .bind(m.status.to_i64())
    .bind(m.edited)
    .bind(m.deleted)
    .bind(quote.map(|q| &q.author))
    .bind(quote.map(|q| q.sent_at))
    .bind(quote_text)
    .bind(kind)
    .bind(kind_text)
    .bind(i64::from(m.expires_in))
    .bind(m.expires_at)
    .execute(&mut *conn)
    .await?;
    let inserted = res.rows_affected() == 1;
    if inserted {
        // every message carries its chat's current timer: the newest one says what it is now
        sqlx::query("UPDATE conversations SET expire_timer = ?2, timer_at = ?3 WHERE id = ?1 AND timer_at <= ?3")
            .bind(&m.conversation_id)
            .bind(i64::from(m.expires_in))
            .bind(m.sent_at)
            .execute(&mut *conn)
            .await?;
        apply_pending(conn, &m.key()).await?;
        refresh_summary(conn, &m.conversation_id).await?;
    }
    Ok(inserted)
}

/// Copies files into `dir` under temporary names; all or none. Returns each name and size.
fn stage_copies(dir: &std::path::Path, sent_at: i64, sources: &[String]) -> std::io::Result<Vec<(String, u64)>> {
    std::fs::create_dir_all(dir)?;
    let mut staged = vec![];
    for (i, source) in sources.iter().enumerate() {
        let name = format!("{sent_at}-{i}.part");
        match std::fs::copy(source, dir.join(&name)) {
            Ok(size) => staged.push((name, size)),
            Err(e) => {
                let _ = std::fs::remove_file(dir.join(&name));
                for (n, _) in &staged {
                    let _ = std::fs::remove_file(dir.join(n));
                }
                return Err(e);
            }
        }
    }
    Ok(staged)
}

/// The row of one of our files (Ready, its copy named by the returned id once renamed).
async fn add_outgoing_file(
    conn: &mut SqliteConnection,
    m: &Message,
    f: &OutgoingFile,
    position: i64,
    size: u64,
) -> Result<i64, sqlx::Error> {
    let name = std::path::Path::new(&f.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    sqlx::query_scalar(
        "INSERT INTO attachments(message_author, message_sent_at, position, content_type, file_name, size,
           width, height, is_voice_note, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING id",
    )
    .bind(&m.author)
    .bind(m.sent_at)
    .bind(position)
    .bind(&f.content_type)
    .bind(name)
    .bind(size as i64)
    .bind(f.width)
    .bind(f.height)
    .bind(f.is_voice_note)
    .bind(AttachmentState::Ready.to_i64())
    .fetch_one(&mut *conn)
    .await
}

/// Starts a read message's disappearing timer (once).
async fn start_timer(conn: &mut SqliteConnection, key: &MessageKey) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE messages SET expires_at = ?3 + expires_in * 1000
         WHERE author = ?1 AND sent_at = ?2 AND expires_in > 0 AND expires_at IS NULL",
    )
    .bind(&key.author)
    .bind(key.sent_at)
    .bind(chrono::Utc::now().timestamp_millis())
    .execute(conn)
    .await?;
    Ok(())
}

/// Applies ops that arrived before this (just inserted) message.
async fn apply_pending(conn: &mut SqliteConnection, key: &MessageKey) -> Result<(), sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM pending_deletes WHERE author = ?1 AND sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        == 1;
    if deleted {
        sqlx::query(
            "UPDATE messages SET body = NULL, quote_text = NULL, deleted = 1 WHERE author = ?1 AND sent_at = ?2",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
        // nothing is attached yet: attachments are added after the message
        forget_deleted_text(conn, key).await?;
    }
    let status: Option<i64> =
        sqlx::query_scalar("DELETE FROM pending_status WHERE author = ?1 AND sent_at = ?2 RETURNING status")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(status) = status {
        sqlx::query("UPDATE messages SET status = MAX(status, ?3) WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .bind(status)
            .execute(&mut *conn)
            .await?;
        if status >= MessageStatus::Read.to_i64() {
            start_timer(conn, key).await?;
        }
    }
    Ok(())
}

/// The conversation a stored message belongs to, if it is stored.
async fn conversation_of(
    conn: &mut SqliteConnection,
    key: &MessageKey,
) -> Result<Option<(String, MessageKey)>, sqlx::Error> {
    let conv: Option<String> =
        sqlx::query_scalar("SELECT conversation_id FROM messages WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(conn)
            .await?;
    Ok(conv.map(|c| (c, key.clone())))
}

/// Recomputes a conversation's last message time, preview and unread count from its messages.
/// Every op that changes a `messages` row calls this inside its transaction (reactions don't
/// affect the summary).
pub(crate) async fn refresh_summary(conn: &mut SqliteConnection, conversation_id: &str) -> Result<(), sqlx::Error> {
    // A message that couldn't be decrypted counts as the chat's latest activity, so the chat
    // shows (and moves up) even if it has no message yet.
    sqlx::query(
        "UPDATE conversations SET
           last_message_at = MAX(
             COALESCE((SELECT MAX(sent_at) FROM messages WHERE conversation_id = ?1), 0),
             COALESCE((SELECT MAX(at) FROM chat_events WHERE conversation_id = ?1 AND kind = ?2), 0)),
           last_message_preview = CASE
             WHEN COALESCE((SELECT MAX(at) FROM chat_events WHERE conversation_id = ?1 AND kind = ?2), 0)
                  > COALESCE((SELECT MAX(sent_at) FROM messages WHERE conversation_id = ?1), 0)
             THEN ?3
             ELSE COALESCE((SELECT body FROM messages WHERE conversation_id = ?1
                            ORDER BY sent_at DESC, author DESC LIMIT 1), '')
           END,
           unread_count = (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1
                           AND is_outgoing = 0 AND deleted = 0 AND status < 3)
         WHERE id = ?1",
    )
    .bind(conversation_id)
    .bind(ChatEventKind::DecryptionFailed.to_i64())
    .bind(UNDECRYPTABLE_PREVIEW)
    .execute(conn)
    .await?;
    Ok(())
}

fn kind_of(conversation_id: &str) -> Result<ConversationKind, sqlx::Error> {
    ConversationKind::from_id(conversation_id)
        .ok_or_else(|| sqlx::Error::InvalidArgument(format!("bad conversation id: {conversation_id:?}")))
}

fn kind_to_i64(kind: ConversationKind) -> i64 {
    match kind {
        ConversationKind::Direct => 0,
        ConversationKind::Group => 1,
    }
}

fn decode_err(what: String) -> sqlx::Error {
    sqlx::Error::Decode(what.into())
}

fn row_to_conversation(r: &SqliteRow) -> Result<Conversation, sqlx::Error> {
    let id: String = r.try_get("id")?;
    let kind = kind_of(&id)?;
    let unread: i64 = r.try_get("unread_count")?;
    // the latest thing is a message that couldn't be decrypted: the row is about that only
    let failed_at: Option<i64> = r.try_get("failed_at")?;
    let last_sent_at: Option<i64> = r.try_get("last_sent_at")?;
    let undecryptable = failed_at.is_some_and(|f| last_sent_at.is_none_or(|m| f > m));
    let status: Option<i64> = r.try_get("last_status")?;
    let status = status.unwrap_or(MessageStatus::Sent.to_i64());
    Ok(Conversation {
        last_message_undecryptable: undecryptable,
        expire_timer: u32::try_from(r.try_get::<i64, _>("expire_timer")?).unwrap_or(0),
        last_message_kind: match r.try_get::<Option<i64>, _>("last_kind")? {
            Some(kind) if !undecryptable => message_kind(kind, r.try_get("last_kind_text")?)?,
            _ => MessageKind::Normal,
        },
        last_message_author: if undecryptable {
            String::new()
        } else {
            r.try_get::<Option<String>, _>("last_author")?.unwrap_or_default()
        },
        last_message_outgoing: !undecryptable && r.try_get::<Option<bool>, _>("last_outgoing")?.unwrap_or(false),
        last_message_status: MessageStatus::from_i64(status)
            .ok_or_else(|| decode_err(format!("message status {status}")))?,
        last_message_deleted: !undecryptable && r.try_get::<Option<bool>, _>("last_deleted")?.unwrap_or(false),
        last_message_file: match r.try_get::<Option<String>, _>("file_type")?.filter(|_| !undecryptable) {
            Some(content_type) => Some(FileSummary {
                content_type,
                file_name: r.try_get("file_name")?,
                is_voice_note: r.try_get::<Option<bool>, _>("file_voice")?.unwrap_or(false),
            }),
            None => None,
        },
        kind,
        title: r.try_get("title")?,
        last_message_at: r.try_get("last_message_at")?,
        last_message_preview: r.try_get("last_message_preview")?,
        unread_count: u32::try_from(unread).map_err(|_| decode_err(format!("unread_count {unread}")))?,
        id,
    })
}

fn row_to_message(r: &SqliteRow) -> Result<Message, sqlx::Error> {
    let status: i64 = r.try_get("status")?;
    let quote_author: Option<String> = r.try_get("quote_author")?;
    let quote_sent_at: Option<i64> = r.try_get("quote_sent_at")?;
    Ok(Message {
        conversation_id: r.try_get("conversation_id")?,
        author: r.try_get("author")?,
        sent_at: r.try_get("sent_at")?,
        received_at: r.try_get("received_at")?,
        body: r.try_get("body")?,
        is_outgoing: r.try_get("is_outgoing")?,
        status: MessageStatus::from_i64(status).ok_or_else(|| decode_err(format!("message status {status}")))?,
        edited: r.try_get("edited")?,
        deleted: r.try_get("deleted")?,
        quote: match (quote_author, quote_sent_at) {
            (Some(author), Some(sent_at)) => Some(Quote {
                author,
                sent_at,
                text: r.try_get("quote_text")?,
            }),
            _ => None,
        },
        kind: message_kind(r.try_get("kind")?, r.try_get("kind_text")?)?,
        expires_in: u32::try_from(r.try_get::<i64, _>("expires_in")?).unwrap_or(0),
        expires_at: r.try_get("expires_at")?,
    })
}

fn message_kind(kind: i64, text: Option<String>) -> Result<MessageKind, sqlx::Error> {
    MessageKind::from_row(kind, text).ok_or_else(|| decode_err(format!("message kind {kind}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> ProjectionDb {
        ProjectionDb::open("sqlite::memory:", None).await.unwrap()
    }

    fn msg(author: &str, sent_at: i64, body: &str) -> Message {
        Message {
            conversation_id: "c:alice".into(),
            author: author.into(),
            sent_at,
            received_at: sent_at,
            body: Some(body.into()),
            is_outgoing: false,
            status: MessageStatus::Delivered,
            edited: false,
            deleted: false,
            quote: None,
            kind: crate::projection::MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
        }
    }

    fn in_conv(conv: &str, m: Message) -> Message {
        Message {
            conversation_id: conv.into(),
            ..m
        }
    }

    fn hit_times(hits: &[SearchHit]) -> Vec<i64> {
        hits.iter().map(|h| h.message.sent_at).collect()
    }

    fn times(ms: &[Message]) -> Vec<i64> {
        ms.iter().map(|m| m.sent_at).collect()
    }

    async fn alice(db: &ProjectionDb) {
        db.upsert_conversation_title("c:alice", "Alice").await.unwrap();
    }

    async fn conv(db: &ProjectionDb, id: &str) -> Conversation {
        db.conversations()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    }

    /// A temp file path removed on drop, even if the test panics.
    struct TempDb(std::path::PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("semaphore-{tag}-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&p);
            Self(p)
        }
        fn url(&self) -> String {
            format!("sqlite://{}", self.0.display())
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn insert_and_page_messages() {
        let db = db().await;
        for t in 1..=5 {
            db.insert_message(&msg("alice", t, &format!("m{t}"))).await.unwrap();
        }
        assert_eq!(times(&db.messages("c:alice", None, 2).await.unwrap()), vec![4, 5]);
        let before = MessageKey {
            author: "alice".into(),
            sent_at: 4,
        };
        assert_eq!(
            times(&db.messages("c:alice", Some(before), 2).await.unwrap()),
            vec![2, 3]
        );
        let before = MessageKey {
            author: "alice".into(),
            sent_at: 1,
        };
        assert!(db.messages("c:alice", Some(before), 2).await.unwrap().is_empty());
        assert!(db.messages("c:alice", None, 0).await.unwrap().is_empty());
        assert!(db.messages("c:nobody", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn paging_keeps_messages_with_equal_timestamps() {
        let db = db().await;
        for (a, t) in [("alice", 1), ("bob", 2), ("carol", 2), ("alice", 3)] {
            db.insert_message(&msg(a, t, "x")).await.unwrap();
        }
        let first = db.messages("c:alice", None, 2).await.unwrap();
        let second = db.messages("c:alice", Some(first[0].key()), 2).await.unwrap();
        let all: Vec<_> = second
            .iter()
            .chain(&first)
            .map(|m| (m.author.as_str(), m.sent_at))
            .collect();
        assert_eq!(all, vec![("alice", 1), ("bob", 2), ("carol", 2), ("alice", 3)]);
    }

    #[tokio::test]
    async fn messages_are_scoped_to_their_conversation() {
        let db = db().await;
        for (c, t) in [("c:alice", 1), ("c:bob", 2), ("c:alice", 3), ("c:bob", 4)] {
            db.insert_message(&in_conv(c, msg("x", t, "m"))).await.unwrap();
        }
        assert_eq!(times(&db.messages("c:alice", None, 10).await.unwrap()), vec![1, 3]);
        let before = MessageKey {
            author: "x".into(),
            sent_at: 4,
        };
        assert_eq!(times(&db.messages("c:bob", Some(before), 10).await.unwrap()), vec![2]);
    }

    #[tokio::test]
    async fn message_round_trips_all_fields() {
        let db = db().await;
        let m = Message {
            is_outgoing: true,
            status: MessageStatus::Failed,
            edited: true,
            quote: Some(Quote {
                author: "bob".into(),
                sent_at: 9,
                text: Some("q".into()),
            }),
            ..msg("me", 10, "hi")
        };
        let no_quote_text = Message {
            quote: Some(Quote {
                author: "bob".into(),
                sent_at: 8,
                text: None,
            }),
            ..msg("me", 11, "a")
        };
        db.insert_message(&m).await.unwrap();
        db.insert_message(&no_quote_text).await.unwrap();
        assert_eq!(db.messages("c:alice", None, 10).await.unwrap(), vec![m, no_quote_text]);
    }

    #[tokio::test]
    async fn every_status_round_trips() {
        let db = db().await;
        for (i, s) in MessageStatus::ALL.into_iter().enumerate() {
            db.insert_message(&Message {
                status: s,
                ..msg("a", i as i64, "x")
            })
            .await
            .unwrap();
        }
        let got: Vec<_> = db
            .messages("c:alice", None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.status)
            .collect();
        assert_eq!(got, MessageStatus::ALL);
    }

    #[tokio::test]
    async fn deleted_message_is_stored_without_text_and_hidden_from_search() {
        let db = db().await;
        let deleted = Message {
            deleted: true,
            quote: Some(Quote {
                author: "b".into(),
                sent_at: 1,
                text: Some("quoted secret".into()),
            }),
            ..msg("alice", 1, "secret plan")
        };
        db.insert_message(&deleted).await.unwrap();
        db.insert_message(&msg("alice", 2, "secret too")).await.unwrap();
        let stored = &db.messages("c:alice", None, 10).await.unwrap()[0];
        assert!(stored.deleted && stored.body.is_none());
        assert_eq!(stored.quote.as_ref().unwrap().text, None);
        assert_eq!(hit_times(&db.search("secret", None, 10).await.unwrap()), vec![2]);
        assert_eq!(conv(&db, "c:alice").await.unread_count, 1);
    }

    #[tokio::test]
    async fn duplicate_message_is_ignored_and_not_double_counted() {
        let db = db().await;
        assert!(db.insert_message(&msg("alice", 10, "hi")).await.unwrap());
        assert!(!db.insert_message(&msg("alice", 10, "changed")).await.unwrap());
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (c.unread_count, c.last_message_preview.as_str(), c.last_message_at),
            (1, "hi", 10)
        );
        assert_eq!(
            db.messages("c:alice", None, 10).await.unwrap()[0].body.as_deref(),
            Some("hi")
        );
    }

    #[tokio::test]
    async fn same_timestamp_from_different_authors_are_distinct() {
        let db = db().await;
        assert!(db.insert_message(&msg("alice", 10, "a")).await.unwrap());
        assert!(db.insert_message(&msg("bob", 10, "b")).await.unwrap());
        assert_eq!(db.messages("c:alice", None, 10).await.unwrap().len(), 2);
        assert_eq!(conv(&db, "c:alice").await.unread_count, 2);
    }

    #[tokio::test]
    async fn unread_counts_only_incoming_unread() {
        let db = db().await;
        for t in 1..=3 {
            db.insert_message(&msg("alice", t, "in")).await.unwrap();
        }
        db.insert_message(&Message {
            is_outgoing: true,
            ..msg("me", 4, "out")
        })
        .await
        .unwrap();
        db.insert_message(&Message {
            status: MessageStatus::Read,
            ..msg("alice", 5, "seen")
        })
        .await
        .unwrap();
        assert_eq!(conv(&db, "c:alice").await.unread_count, 3);
    }

    #[tokio::test]
    async fn older_message_does_not_replace_preview() {
        let db = db().await;
        db.insert_message(&msg("alice", 10, "new")).await.unwrap();
        db.insert_message(&msg("alice", 5, "old")).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.last_message_at, c.last_message_preview.as_str()), (10, "new"));
    }

    #[tokio::test]
    async fn newest_message_without_text_clears_preview() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        db.insert_message(&Message {
            body: None,
            ..msg("alice", 2, "")
        })
        .await
        .unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.last_message_at, c.last_message_preview.as_str()), (2, ""));
        assert_eq!(db.search("hi", None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn conversations_ordered_by_recency_then_id() {
        let db = db().await;
        alice(&db).await;
        db.upsert_conversation_title("c:bob", "Bob").await.unwrap();
        db.upsert_conversation_title("c:carol", "Carol").await.unwrap();
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "b"))).await.unwrap();
        let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        // carol has a name but no messages: not listed
        assert_eq!(ids, vec!["c:bob", "c:alice"]);
    }

    #[tokio::test]
    async fn message_for_unknown_conversation_creates_it_with_kind_from_id() {
        let db = db().await;
        db.insert_message(&in_conv("g:abcd", msg("bob", 1, "x"))).await.unwrap();
        let c = conv(&db, "g:abcd").await;
        assert_eq!((c.kind, c.title.as_str()), (ConversationKind::Group, ""));
    }

    #[tokio::test]
    async fn titles_update_but_empty_never_overwrites() {
        let db = db().await;
        alice(&db).await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.upsert_conversation_title("c:alice", "Alice Smith").await.unwrap();
        db.upsert_conversation_title("c:alice", "").await.unwrap();
        assert_eq!(conv(&db, "c:alice").await.title, "Alice Smith");
        db.insert_message(&in_conv("c:new", msg("n", 1, "x"))).await.unwrap();
        db.upsert_conversation_title("c:new", "New").await.unwrap();
        assert_eq!(conv(&db, "c:new").await.title, "New");
    }

    #[tokio::test]
    async fn bad_conversation_ids_are_rejected() {
        let db = db().await;
        for bad in ["", "alice", "x:1", "G:00"] {
            assert!(db.upsert_conversation_title(bad, "t").await.is_err(), "{bad:?}");
            assert!(
                db.insert_message(&in_conv(bad, msg("a", 1, "x"))).await.is_err(),
                "{bad:?}"
            );
        }
        assert!(db.conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts5_search_finds_words_newest_first_with_limit() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "coffee at noon")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "more coffee")))
            .await
            .unwrap();
        db.insert_message(&msg("alice", 3, "COFFEE again")).await.unwrap();
        db.insert_message(&msg("alice", 4, "unrelated")).await.unwrap();
        assert_eq!(hit_times(&db.search("coffee", None, 2).await.unwrap()), vec![3, 2]);
        assert_eq!(hit_times(&db.search("coffee", None, 10).await.unwrap()), vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn search_folds_case_and_diacritics() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "Café LUNCH")).await.unwrap();
        assert_eq!(db.search("cafe", None, 10).await.unwrap().len(), 1);
        assert_eq!(db.search("lunch", None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_treats_fts_syntax_as_text() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "say \"hi\" (now) AND NOT -x*"))
            .await
            .unwrap();
        db.insert_message(&msg("alice", 2, "plain words")).await.unwrap();
        assert_eq!(hit_times(&db.search("\"hi\"", None, 10).await.unwrap()), vec![1]);
        assert_eq!(hit_times(&db.search("(now)", None, 10).await.unwrap()), vec![1]);
        assert_eq!(hit_times(&db.search("AND NOT", None, 10).await.unwrap()), vec![1]);
        assert!(db.search("-x*", None, 10).await.is_ok());
        // a lone quote is plain text too: it finds the message that has one
        assert_eq!(hit_times(&db.search("\"", None, 10).await.unwrap()), vec![1]);
        assert!(db.search("   ", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts_follows_body_updates_and_row_deletes() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "old words")).await.unwrap();
        sqlx::query("UPDATE messages SET body = 'new words' WHERE author = 'alice' AND sent_at = 1")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.search("old", None, 10).await.unwrap().is_empty());
        assert_eq!(db.search("new", None, 10).await.unwrap().len(), 1);
        sqlx::query("DELETE FROM messages").execute(&db.pool).await.unwrap();
        db.insert_message(&msg("bob", 2, "fresh")).await.unwrap(); // may reuse the deleted rowid
        assert!(db.search("new", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bad_rows_are_errors_not_panics() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        // Bypass the CHECK constraints the way a corrupt file could.
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET status = 9")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.messages("c:alice", None, 10).await.is_err());
    }

    #[tokio::test]
    async fn encrypted_file_reopens_with_key_and_rejects_wrong_or_missing_key() {
        let f = TempDb::new("proj-enc");
        let key = "ab".repeat(32);
        {
            let db = ProjectionDb::open(&f.url(), Some(&key)).await.unwrap();
            db.insert_message(&msg("alice", 1, "kept")).await.unwrap();
        }
        assert!(!std::fs::read(&f.0).unwrap().starts_with(b"SQLite format 3\0"));
        let db = ProjectionDb::open(&f.url(), Some(&key)).await.unwrap();
        assert_eq!(db.conversations().await.unwrap().len(), 1);
        drop(db);
        assert!(ProjectionDb::open(&f.url(), Some(&"cd".repeat(32))).await.is_err());
        assert!(ProjectionDb::open(&f.url(), None).await.is_err());
    }

    #[tokio::test]
    async fn malformed_keys_are_rejected_before_creating_a_file() {
        let f = TempDb::new("proj-badkey");
        let quote_key = format!("{}'--", "a".repeat(60));
        for bad in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "zz".repeat(32),
            quote_key,
        ] {
            let err = ProjectionDb::open(&f.url(), Some(&bad)).await.err().expect("must fail");
            assert!(matches!(err, sqlx::Error::Configuration(_)), "{bad:?}: {err}");
        }
        assert!(!f.0.exists());
    }

    fn k(author: &str, sent_at: i64) -> MessageKey {
        MessageKey {
            author: author.into(),
            sent_at,
        }
    }

    fn changed(conv: &str, author: &str, sent_at: i64) -> Vec<Applied> {
        vec![Applied::MessageChanged {
            conversation_id: conv.into(),
            key: k(author, sent_at),
        }]
    }

    #[tokio::test]
    async fn apply_insert_reports_new_messages_only() {
        let db = db().await;
        let m = msg("alice", 1, "hi");
        assert_eq!(
            db.apply(&Op::InsertMessage(m.clone())).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        assert!(db.apply(&Op::InsertMessage(m)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reactions_add_replace_remove() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        let react = |who: &str, e: Option<&str>| Op::React {
            target: k("alice", 1),
            reactor: who.into(),
            emoji: e.map(Into::into),
        };
        assert_eq!(
            db.apply(&react("me", Some("👍"))).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        db.apply(&react("bob", Some("😂"))).await.unwrap();
        db.apply(&react("me", Some("❤️"))).await.unwrap(); // one reaction per reactor
        let r = db.reactions(&k("alice", 1)).await.unwrap();
        assert_eq!(
            r,
            vec![
                Reaction {
                    reactor: "bob".into(),
                    emoji: "😂".into()
                },
                Reaction {
                    reactor: "me".into(),
                    emoji: "❤️".into()
                }
            ]
        );
        assert_eq!(
            db.apply(&react("me", None)).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        assert_eq!(
            db.reactions(&k("alice", 1)).await.unwrap(),
            vec![Reaction {
                reactor: "bob".into(),
                emoji: "😂".into()
            }]
        );
    }

    #[tokio::test]
    async fn reaction_arriving_before_its_message_shows_once_it_arrives() {
        let db = db().await;
        let op = Op::React {
            target: k("alice", 1),
            reactor: "bob".into(),
            emoji: Some("👍".into()),
        };
        assert!(db.apply(&op).await.unwrap().is_empty());
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert_eq!(db.reactions(&k("alice", 1)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn edit_replaces_body_keeps_latest_and_updates_search_and_preview() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "orignal typo")).await.unwrap();
        let edit = |body: &str, at| Op::Edit {
            target: k("alice", 1),
            new_body: Some(body.into()),
            edited_at: at,
        };
        assert_eq!(
            db.apply(&edit("second", 20)).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        assert!(db.apply(&edit("first", 10)).await.unwrap().is_empty()); // older edit arriving late
        let m = db.message(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!((m.body.as_deref(), m.edited), (Some("second"), true));
        assert_eq!(db.search("second", None, 5).await.unwrap().len(), 1);
        assert!(db.search("typo", None, 5).await.unwrap().is_empty());
        assert_eq!(conv(&db, "c:alice").await.last_message_preview, "second");
    }

    #[tokio::test]
    async fn only_the_author_can_edit_or_delete() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "mine")).await.unwrap();
        // an op from bob targets (bob, 1), which is not alice's message
        let edit = Op::Edit {
            target: k("bob", 1),
            new_body: Some("hacked".into()),
            edited_at: 5,
        };
        assert!(db.apply(&edit).await.unwrap().is_empty());
        assert!(db.apply(&Op::Delete { target: k("bob", 1) }).await.unwrap().is_empty());
        assert_eq!(
            db.message(&k("alice", 1)).await.unwrap().unwrap().body.as_deref(),
            Some("mine")
        );
    }

    #[tokio::test]
    async fn delete_tombstones_clears_text_reactions_search_and_preview_and_blocks_edits() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "older")).await.unwrap();
        db.insert_message(&Message {
            quote: Some(Quote {
                author: "me".into(),
                sent_at: 0,
                text: Some("quoted".into()),
            }),
            ..msg("alice", 2, "secret")
        })
        .await
        .unwrap();
        db.apply(&Op::React {
            target: k("alice", 2),
            reactor: "me".into(),
            emoji: Some("👍".into()),
        })
        .await
        .unwrap();
        assert_eq!(
            db.apply(&Op::Delete { target: k("alice", 2) }).await.unwrap(),
            changed("c:alice", "alice", 2)
        );
        let m = db.message(&k("alice", 2)).await.unwrap().unwrap();
        assert!(m.deleted && m.body.is_none() && m.quote.unwrap().text.is_none());
        assert!(db.reactions(&k("alice", 2)).await.unwrap().is_empty());
        assert!(db.search("secret", None, 5).await.unwrap().is_empty());
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.last_message_preview.as_str(), c.unread_count), ("", 1));
        let edit = Op::Edit {
            target: k("alice", 2),
            new_body: Some("back".into()),
            edited_at: 9,
        };
        assert!(db.apply(&edit).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_only_moves_up_and_read_clears_unread() {
        let db = db().await;
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", 1, "out")
        })
        .await
        .unwrap();
        db.insert_message(&msg("alice", 2, "in")).await.unwrap();
        db.insert_message(&msg("alice", 3, "in")).await.unwrap();
        let set = |targets, status| Op::SetStatus { targets, status };
        db.apply(&set(vec![k("me", 1)], MessageStatus::Read)).await.unwrap();
        assert!(db
            .apply(&set(vec![k("me", 1)], MessageStatus::Delivered))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            db.message(&k("me", 1)).await.unwrap().unwrap().status,
            MessageStatus::Read
        );

        assert_eq!(conv(&db, "c:alice").await.unread_count, 2);
        let got = db
            .apply(&set(
                vec![k("alice", 2), k("alice", 3), k("nobody", 9)],
                MessageStatus::Read,
            ))
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(conv(&db, "c:alice").await.unread_count, 0);
    }

    #[tokio::test]
    async fn typing_passes_through() {
        let db = db().await;
        let op = Op::Typing {
            conversation_id: "c:alice".into(),
            who: "alice".into(),
            started: true,
        };
        assert_eq!(
            db.apply(&op).await.unwrap(),
            vec![Applied::Typing {
                conversation_id: "c:alice".into(),
                who: "alice".into(),
                started: true
            }]
        );
    }

    async fn fts_ok(db: &ProjectionDb) {
        sqlx::query("INSERT INTO messages_fts(messages_fts) VALUES ('integrity-check')")
            .execute(&db.pool)
            .await
            .expect("FTS index out of sync");
    }

    async fn raw_fts_hits(db: &ProjectionDb, word: &str) -> usize {
        sqlx::query("SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1")
            .bind(word)
            .fetch_all(&db.pool)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn ops_can_target_the_latest_edit_revision() {
        let db = db().await;
        db.insert_message(&msg("alice", 100, "v1")).await.unwrap();
        let edit = |target, body: &str, at| Op::Edit {
            target: k("alice", target),
            new_body: Some(body.into()),
            edited_at: at,
        };
        db.apply(&edit(100, "v2", 200)).await.unwrap();
        // the second edit targets the first edit's timestamp
        assert_eq!(
            db.apply(&edit(200, "v3", 300)).await.unwrap(),
            changed("c:alice", "alice", 100)
        );
        assert_eq!(
            db.message(&k("alice", 100)).await.unwrap().unwrap().body.as_deref(),
            Some("v3")
        );
        // reaction and read state addressed to a revision land on the message
        db.apply(&Op::React {
            target: k("alice", 300),
            reactor: "me".into(),
            emoji: Some("👍".into()),
        })
        .await
        .unwrap();
        assert_eq!(db.reactions(&k("alice", 100)).await.unwrap().len(), 1);
        db.apply(&Op::SetStatus {
            targets: vec![k("alice", 300)],
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        assert_eq!(conv(&db, "c:alice").await.unread_count, 0);
        // delete for everyone addressed to a revision
        assert_eq!(
            db.apply(&Op::Delete {
                target: k("alice", 200)
            })
            .await
            .unwrap(),
            changed("c:alice", "alice", 100)
        );
        assert!(db.message(&k("alice", 100)).await.unwrap().unwrap().deleted);
        fts_ok(&db).await;
        assert_eq!(raw_fts_hits(&db, "v3").await, 0);
    }

    #[tokio::test]
    async fn receipts_for_an_edit_of_my_message_count() {
        let db = db().await;
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", 10, "a")
        })
        .await
        .unwrap();
        db.apply(&Op::Edit {
            target: k("me", 10),
            new_body: Some("b".into()),
            edited_at: 20,
        })
        .await
        .unwrap();
        db.apply(&Op::SetStatus {
            targets: vec![k("me", 20)],
            status: MessageStatus::Delivered,
        })
        .await
        .unwrap();
        assert_eq!(
            db.message(&k("me", 10)).await.unwrap().unwrap().status,
            MessageStatus::Delivered
        );
    }

    #[tokio::test]
    async fn reaction_after_delete_is_dropped() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        let op = Op::React {
            target: k("alice", 1),
            reactor: "bob".into(),
            emoji: Some("👍".into()),
        };
        assert!(db.apply(&op).await.unwrap().is_empty());
        assert!(db.reactions(&k("alice", 1)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_before_its_message_hides_it_when_it_arrives() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "older")).await.unwrap();
        assert!(db
            .apply(&Op::Delete { target: k("alice", 2) })
            .await
            .unwrap()
            .is_empty());
        db.insert_message(&msg("alice", 2, "secret")).await.unwrap();
        let m = db.message(&k("alice", 2)).await.unwrap().unwrap();
        assert!(m.deleted && m.body.is_none());
        assert!(db.search("secret", None, 5).await.unwrap().is_empty());
        assert_eq!(raw_fts_hits(&db, "secret").await, 0);
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.last_message_preview.as_str(), c.unread_count), ("", 1));
        fts_ok(&db).await;
    }

    #[tokio::test]
    async fn read_on_phone_before_message_arrives_keeps_it_read() {
        let db = db().await;
        db.apply(&Op::SetStatus {
            targets: vec![k("alice", 1)],
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert_eq!(
            db.message(&k("alice", 1)).await.unwrap().unwrap().status,
            MessageStatus::Read
        );
        assert_eq!(conv(&db, "c:alice").await.unread_count, 0);
    }

    #[tokio::test]
    async fn delete_clears_quotes_of_the_deleted_text() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "secret")).await.unwrap();
        db.insert_message(&Message {
            quote: Some(Quote {
                author: "alice".into(),
                sent_at: 1,
                text: Some("secret".into()),
            }),
            ..msg("bob", 2, "replying")
        })
        .await
        .unwrap();
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        let reply = db.message(&k("bob", 2)).await.unwrap().unwrap();
        assert_eq!(
            reply.quote,
            Some(Quote {
                author: "alice".into(),
                sent_at: 1,
                text: None
            })
        );
    }

    #[tokio::test]
    async fn null_body_edit_keeps_fts_consistent() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "caption words")).await.unwrap();
        db.apply(&Op::Edit {
            target: k("alice", 1),
            new_body: None,
            edited_at: 5,
        })
        .await
        .unwrap();
        let m = db.message(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!((m.body, m.edited), (None, true));
        assert_eq!(conv(&db, "c:alice").await.last_message_preview, "");
        assert_eq!(raw_fts_hits(&db, "caption").await, 0);
        fts_ok(&db).await;
    }

    #[tokio::test]
    async fn repeated_ops_report_no_change() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        let edit = Op::Edit {
            target: k("alice", 1),
            new_body: Some("y".into()),
            edited_at: 5,
        };
        let react = Op::React {
            target: k("alice", 1),
            reactor: "me".into(),
            emoji: Some("👍".into()),
        };
        let read = Op::SetStatus {
            targets: vec![k("alice", 1)],
            status: MessageStatus::Read,
        };
        let unreact = Op::React {
            target: k("alice", 1),
            reactor: "me".into(),
            emoji: None,
        };
        let delete = Op::Delete { target: k("alice", 1) };
        for op in [edit, react, read, unreact, delete] {
            assert_eq!(db.apply(&op).await.unwrap(), changed("c:alice", "alice", 1), "{op:?}");
            assert!(db.apply(&op).await.unwrap().is_empty(), "repeated {op:?}");
        }
    }

    #[tokio::test]
    async fn sent_message_moves_to_delivered() {
        let db = db().await;
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", 1, "x")
        })
        .await
        .unwrap();
        assert_eq!(
            db.apply(&Op::SetStatus {
                targets: vec![k("me", 1)],
                status: MessageStatus::Delivered
            })
            .await
            .unwrap(),
            changed("c:alice", "me", 1)
        );
        assert_eq!(
            db.message(&k("me", 1)).await.unwrap().unwrap().status,
            MessageStatus::Delivered
        );
    }

    #[tokio::test]
    async fn outgoing_pending_message_then_failed_then_retried() {
        let db = db().await;
        let m = Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            ..msg("me", 5, "out")
        };
        db.insert_message(&m).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.last_message_preview.as_str(), c.unread_count), ("out", 0));
        db.set_failed(&k("me", 5)).await.unwrap();
        assert_eq!(
            db.message(&k("me", 5)).await.unwrap().unwrap().status,
            MessageStatus::Failed
        );
        assert!(db.set_pending(&k("me", 5)).await.unwrap());
        assert!(!db.set_pending(&k("me", 5)).await.unwrap()); // only from Failed
        db.apply(&Op::SetStatus {
            targets: vec![k("me", 5)],
            status: MessageStatus::Sent,
        })
        .await
        .unwrap();
        db.set_failed(&k("me", 5)).await.unwrap(); // a sent message never becomes failed
        assert_eq!(
            db.message(&k("me", 5)).await.unwrap().unwrap().status,
            MessageStatus::Sent
        );
    }

    #[tokio::test]
    async fn title_reads_hidden_contact_names() {
        let db = db().await;
        db.upsert_conversation_title("c:bob", "Bob").await.unwrap();
        assert_eq!(db.title("c:bob").await.unwrap().as_deref(), Some("Bob"));
        assert_eq!(db.title("c:nobody").await.unwrap(), None);
    }

    #[tokio::test]
    async fn stale_pending_messages_fail_and_receipts_can_still_lift_them() {
        let db = db().await;
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            ..msg("me", 1, "a")
        })
        .await
        .unwrap();
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", 2, "b")
        })
        .await
        .unwrap();
        db.insert_message(&msg("alice", 3, "in")).await.unwrap();
        let failed = db.fail_all_pending().await.unwrap();
        assert_eq!(failed, vec![("c:alice".to_string(), k("me", 1))]);
        assert_eq!(
            db.message(&k("me", 1)).await.unwrap().unwrap().status,
            MessageStatus::Failed
        );
        assert_eq!(
            db.message(&k("me", 2)).await.unwrap().unwrap().status,
            MessageStatus::Sent
        );
        db.apply(&Op::SetStatus {
            targets: vec![k("me", 1)],
            status: MessageStatus::Delivered,
        })
        .await
        .unwrap();
        assert_eq!(
            db.message(&k("me", 1)).await.unwrap().unwrap().status,
            MessageStatus::Delivered
        );
    }

    #[tokio::test]
    async fn unread_lists_what_counts_as_unread_and_marking_it_read_clears_the_badge() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "old")).await.unwrap();
        db.insert_message(&msg("alice", 2, "read on phone")).await.unwrap();
        db.insert_message(&msg("bob", 3, "new")).await.unwrap();
        db.insert_message(&msg("alice", 4, "gone")).await.unwrap();
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", 5, "mine")
        })
        .await
        .unwrap();
        db.insert_message(&in_conv("c:carol", msg("carol", 6, "elsewhere")))
            .await
            .unwrap();
        db.apply(&Op::SetStatus {
            targets: vec![k("alice", 2)],
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        db.apply(&Op::Delete { target: k("alice", 4) }).await.unwrap();
        let unread = db.unread("c:alice").await.unwrap();
        assert_eq!(unread, vec![k("alice", 1), k("bob", 3)]);
        assert_eq!(conv(&db, "c:alice").await.unread_count, 2);
        db.apply(&Op::SetStatus {
            targets: unread,
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        assert!(db.unread("c:alice").await.unwrap().is_empty());
        assert_eq!(conv(&db, "c:alice").await.unread_count, 0);
        assert_eq!(db.unread("c:carol").await.unwrap(), vec![k("carol", 6)]);
    }

    #[tokio::test]
    async fn unsent_receipts_are_kept_until_marked_sent() {
        let db = db().await;
        assert!(db.unsent_receipts().await.unwrap().is_empty());
        db.queue_receipts(&[k("bob", 3), k("alice", 1)]).await.unwrap();
        db.queue_receipts(&[k("alice", 1), k("alice", 2)]).await.unwrap();
        assert_eq!(
            db.unsent_receipts().await.unwrap(),
            vec![k("alice", 1), k("alice", 2), k("bob", 3)]
        );
        db.receipts_sent(&[k("alice", 1), k("bob", 3)]).await.unwrap();
        assert_eq!(db.unsent_receipts().await.unwrap(), vec![k("alice", 2)]);
    }

    #[tokio::test]
    async fn settings_are_unknown_until_the_phone_sends_them_and_merge() {
        let db = db().await;
        assert_eq!(db.settings().await.unwrap(), Settings::default());
        let set = |read_receipts, typing_indicators| Op::Settings {
            read_receipts,
            typing_indicators,
        };
        assert!(db.apply(&set(Some(false), None)).await.unwrap().is_empty());
        db.apply(&set(None, Some(true))).await.unwrap();
        assert_eq!(
            db.settings().await.unwrap(),
            Settings {
                read_receipts: Some(false),
                typing_indicators: Some(true)
            }
        );
        db.apply(&set(Some(true), Some(false))).await.unwrap();
        assert_eq!(
            db.settings().await.unwrap(),
            Settings {
                read_receipts: Some(true),
                typing_indicators: Some(false)
            }
        );
    }

    #[tokio::test]
    async fn conversations_describe_their_last_message() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (
                c.last_message_author.as_str(),
                c.last_message_outgoing,
                c.last_message_deleted
            ),
            ("alice", false, false)
        );
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            ..msg("me", 2, "mine")
        })
        .await
        .unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (
                c.last_message_author.as_str(),
                c.last_message_outgoing,
                c.last_message_status
            ),
            ("me", true, MessageStatus::Pending)
        );
        db.apply(&Op::SetStatus {
            targets: vec![k("me", 2)],
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        assert_eq!(conv(&db, "c:alice").await.last_message_status, MessageStatus::Read);
        db.apply(&Op::Delete { target: k("me", 2) }).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert!(c.last_message_deleted && c.last_message_outgoing);
        assert_eq!(c.last_message_preview, "");
    }

    fn new_attachment(content_type: &str, size: u64, voice: bool) -> NewAttachment {
        NewAttachment {
            pointer: vec![1, 2, 3],
            content_type: content_type.into(),
            file_name: Some("a.bin".into()),
            size,
            width: Some(4),
            height: Some(3),
            is_voice_note: voice,
        }
    }

    fn add(key: MessageKey, attachments: Vec<NewAttachment>, preview: Option<NewPreview>) -> Op {
        Op::AddAttachments {
            message: key,
            attachments,
            preview,
        }
    }

    /// A temp folder for attachment files, removed on drop.
    struct TempFiles(std::path::PathBuf);
    impl TempFiles {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("semaphore-files-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
    }
    impl Drop for TempFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn attachments_and_preview_are_stored_once_with_their_message() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "look")).await.unwrap();
        let preview = NewPreview {
            url: "https://example.org".into(),
            title: Some("Example".into()),
            description: None,
            image: Some(new_attachment("image/png", 10, false)),
        };
        let op = add(
            k("alice", 1),
            vec![
                new_attachment("image/jpeg", 100, false),
                new_attachment("application/pdf", 5, false),
            ],
            Some(preview),
        );
        let applied = db.apply(&op).await.unwrap();
        assert_eq!(applied.len(), 3, "{applied:?}");
        assert!(db.apply(&op).await.unwrap().is_empty(), "a duplicate adds nothing");
        let files = db.attachments(&k("alice", 1)).await.unwrap();
        assert_eq!(
            files.iter().map(|a| a.content_type.as_str()).collect::<Vec<_>>(),
            ["image/jpeg", "application/pdf"]
        );
        assert_eq!(files[0].state, AttachmentState::Pending);
        assert_eq!(
            (files[0].width, files[0].height, files[0].path.as_deref()),
            (Some(4), Some(3), None)
        );
        let p = db.link_preview(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!(
            (p.url.as_str(), p.title.as_deref()),
            ("https://example.org", Some("Example"))
        );
        assert_eq!(p.image.unwrap().content_type, "image/png");
        // for messages we don't have (or that were deleted) nothing is stored
        assert!(db
            .apply(&add(k("bob", 9), vec![new_attachment("image/jpeg", 1, false)], None))
            .await
            .unwrap()
            .is_empty());
        assert!(db.link_preview(&k("bob", 9)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn only_small_images_and_voice_notes_download_by_themselves() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        let big = 25 * 1024 * 1024 + 1;
        db.apply(&add(
            k("alice", 1),
            vec![
                new_attachment("image/jpeg", 100, false),
                new_attachment("image/heic", big, false),
                new_attachment("audio/aac", 50, true),
                new_attachment("application/pdf", 5, false),
                new_attachment("video/mp4", 5, false),
            ],
            None,
        ))
        .await
        .unwrap();
        let ids: Vec<i64> = db
            .attachments(&k("alice", 1))
            .await
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(db.auto_downloads().await.unwrap(), vec![ids[0], ids[2]]);
    }

    #[tokio::test]
    async fn downloads_move_through_their_states_and_files_go_with_deleted_messages() {
        let files = TempFiles::new("states");
        let db = db().await.with_files(files.0.clone());
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.apply(&add(
            k("alice", 1),
            vec![
                new_attachment("image/jpeg", 3, false),
                new_attachment("image/jpeg", 3, false),
            ],
            None,
        ))
        .await
        .unwrap();
        let [a, b] = <[Attachment; 2]>::try_from(db.attachments(&k("alice", 1)).await.unwrap()).unwrap();
        let started = db.start_download(a.id).await.unwrap().unwrap();
        assert_eq!(started, ("c:alice".to_string(), k("alice", 1), vec![1, 2, 3]));
        assert!(db.start_download(a.id).await.unwrap().is_none(), "already downloading");
        db.finish_download(a.id, Ok(b"jpg".to_vec())).await.unwrap();
        let a = db.attachment(a.id).await.unwrap().unwrap();
        assert_eq!(a.state, AttachmentState::Ready);
        let path = a.path.clone().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"jpg");
        assert!(db.start_download(a.id).await.unwrap().is_none(), "already there");
        // a corrupt download fails; it can be tried again
        db.start_download(b.id).await.unwrap().unwrap();
        db.finish_download(b.id, Err("bad digest".into())).await.unwrap();
        assert_eq!(
            db.attachment(b.id).await.unwrap().unwrap().state,
            AttachmentState::Failed
        );
        assert!(
            db.auto_downloads().await.unwrap().is_empty(),
            "failed ones wait for the user"
        );
        assert!(db.start_download(b.id).await.unwrap().is_some());
        // an interrupted download starts over after a restart
        db.reset_downloads().await.unwrap();
        assert_eq!(
            db.attachment(b.id).await.unwrap().unwrap().state,
            AttachmentState::Pending
        );
        // deleted for everyone: the rows and the file are gone
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        assert!(db.attachments(&k("alice", 1)).await.unwrap().is_empty());
        assert!(!std::path::Path::new(&path).exists());
    }

    #[tokio::test]
    async fn ids_of_deleted_attachments_are_never_reused() {
        // a download finishing for a deleted attachment must not land on another one's file
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.apply(&add(k("alice", 1), vec![new_attachment("image/jpeg", 3, false)], None))
            .await
            .unwrap();
        let old = db.attachments(&k("alice", 1)).await.unwrap()[0].id;
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        db.insert_message(&msg("alice", 2, "y")).await.unwrap();
        db.apply(&add(k("alice", 2), vec![new_attachment("image/jpeg", 3, false)], None))
            .await
            .unwrap();
        assert_ne!(db.attachments(&k("alice", 2)).await.unwrap()[0].id, old);
    }

    #[tokio::test]
    async fn files_too_big_or_of_unknown_size_never_download() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.apply(&add(
            k("alice", 1),
            vec![
                new_attachment("application/zip", MAX_ATTACHMENT + 1, false),
                new_attachment("application/zip", 0, false),
                new_attachment("application/zip", MAX_ATTACHMENT, false),
            ],
            None,
        ))
        .await
        .unwrap();
        let ids: Vec<i64> = db
            .attachments(&k("alice", 1))
            .await
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect();
        assert!(db.start_download(ids[0]).await.unwrap().is_none());
        assert!(db.start_download(ids[1]).await.unwrap().is_none());
        assert!(db.start_download(ids[2]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_download_finishing_after_its_message_was_deleted_leaves_no_file() {
        let files = TempFiles::new("late");
        let db = db().await.with_files(files.0.clone());
        db.insert_message(&msg("alice", 1, "x")).await.unwrap();
        db.apply(&add(k("alice", 1), vec![new_attachment("image/jpeg", 3, false)], None))
            .await
            .unwrap();
        let id = db.attachments(&k("alice", 1)).await.unwrap()[0].id;
        db.start_download(id).await.unwrap().unwrap();
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        assert_eq!(db.finish_download(id, Ok(b"late".to_vec())).await.unwrap(), None);
        assert!(!files.0.join(id.to_string()).exists());
    }

    #[tokio::test]
    async fn a_send_whose_file_vanished_stores_nothing() {
        let files = TempFiles::new("rollback");
        let src = TempFiles::new("rollback-src");
        std::fs::create_dir_all(&src.0).unwrap();
        let first = src.0.join("first.jpg");
        std::fs::write(&first, b"1").unwrap();
        let db = db().await.with_files(files.0.clone());
        let mine = Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            body: None,
            ..msg("me", 5, "")
        };
        let file = |path: &std::path::Path| OutgoingFile {
            path: path.to_string_lossy().into(),
            content_type: "image/jpeg".into(),
            width: None,
            height: None,
            is_voice_note: false,
        };
        let result = db
            .insert_outgoing(&mine, &[file(&first), file(&src.0.join("gone.jpg"))])
            .await;
        assert!(result.is_err());
        assert!(db.message(&k("me", 5)).await.unwrap().is_none());
        assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 0, "no copies left behind");
        // and with the file there, everything is stored
        assert!(db.insert_outgoing(&mine, &[file(&first)]).await.unwrap());
        let stored = db.attachments(&k("me", 5)).await.unwrap();
        assert_eq!((stored.len(), stored[0].size), (1, 1));
        assert_eq!(std::fs::read(stored[0].path.as_ref().unwrap()).unwrap(), b"1");
    }

    #[tokio::test]
    async fn conversations_say_what_kind_of_file_the_last_message_has() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert_eq!(conv(&db, "c:alice").await.last_message_file, None);
        db.insert_message(&Message {
            body: None,
            ..msg("alice", 2, "")
        })
        .await
        .unwrap();
        db.apply(&add(
            k("alice", 2),
            vec![
                new_attachment("audio/aac", 5, true),
                new_attachment("image/png", 5, false),
            ],
            None,
        ))
        .await
        .unwrap();
        assert_eq!(
            conv(&db, "c:alice").await.last_message_file,
            Some(FileSummary {
                content_type: "audio/aac".into(),
                file_name: Some("a.bin".into()),
                is_voice_note: true
            })
        );
    }

    /// The highlighted part of a snippet, between the markers.
    fn marked(snippet: &str) -> Vec<String> {
        snippet
            .split(HIGHLIGHT_START)
            .skip(1)
            .filter_map(|s| s.split(HIGHLIGHT_END).next().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn the_last_word_matches_as_a_prefix_and_all_words_must_match() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "see you tomorrow at noon"))
            .await
            .unwrap();
        db.insert_message(&msg("alice", 2, "tomorrow then")).await.unwrap();
        assert_eq!(hit_times(&db.search("tomo", None, 10).await.unwrap()), vec![2, 1]);
        assert_eq!(hit_times(&db.search("see tomo", None, 10).await.unwrap()), vec![1]);
        // only the last word is a prefix: "se" alone isn't a word here
        assert!(db.search("se tomorrow", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hits_come_with_a_highlighted_snippet() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "Let's have lunch tomorrow, ok?"))
            .await
            .unwrap();
        let hits = db.search("lunch tom", None, 10).await.unwrap();
        assert_eq!(marked(&hits[0].snippet), ["lunch", "tomorrow"]);
        assert!(hits[0].snippet.contains("Let's have"));
    }

    #[tokio::test]
    async fn search_can_be_limited_to_one_chat() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "coffee?")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "coffee!")))
            .await
            .unwrap();
        assert_eq!(
            hit_times(&db.search("coffee", Some("c:bob"), 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(db.search("coffee", None, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn words_inside_words_cjk_and_emoji_are_found() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "我们明天见")).await.unwrap();
        db.insert_message(&msg("alice", 2, "party 🎉 tonight")).await.unwrap();
        db.insert_message(&msg("alice", 3, "unbelievable")).await.unwrap();
        let cjk = db.search("明天", None, 10).await.unwrap();
        assert_eq!(hit_times(&cjk), vec![1]);
        assert_eq!(marked(&cjk[0].snippet), ["明天"]);
        assert_eq!(hit_times(&db.search("🎉", None, 10).await.unwrap()), vec![2]);
        let inside = db.search("believ", None, 10).await.unwrap();
        assert_eq!(hit_times(&inside), vec![3]);
        // a message found both ways is listed once
        assert_eq!(db.search("unbeliev", None, 10).await.unwrap().len(), 1);
        // deleted messages stay hidden (here the plain-text path; the indexes' in older tests)
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        assert!(db.search("明天", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn newer_messages_are_counted_to_reach_an_old_one() {
        let db = db().await;
        for t in 1..=5 {
            db.insert_message(&msg("alice", t, "x")).await.unwrap();
        }
        db.insert_message(&in_conv("c:bob", msg("bob", 9, "y"))).await.unwrap();
        assert_eq!(db.count_newer("c:alice", &k("alice", 2)).await.unwrap(), 3);
        assert_eq!(db.count_newer("c:alice", &k("alice", 5)).await.unwrap(), 0);
    }

    /// The issue's budget: results in under 50 ms over 100 000 messages (release build:
    /// `cargo test --release -- --ignored search_is_fast`).
    #[tokio::test]
    #[ignore]
    async fn search_is_fast_on_a_hundred_thousand_messages() {
        // as the app has it: an encrypted database file
        let file = TempDb::new("search-speed");
        let db = ProjectionDb::open(&file.url(), Some(&"ab".repeat(32))).await.unwrap();
        let words = [
            "coffee",
            "tomorrow",
            "lunch",
            "meeting",
            "photo",
            "我们明天见",
            "party 🎉",
        ];
        for t in 0..100_000i64 {
            let body = format!(
                "{} number {t} {}",
                words[t as usize % words.len()],
                words[(t as usize / 7) % words.len()]
            );
            db.insert_message(&in_conv(
                &format!("c:{}", t % 50),
                msg(&format!("u{}", t % 50), t, &body),
            ))
            .await
            .unwrap();
        }
        for q in ["coffee", "tomo", "lunch meet", "明天", "🎉", "offe", "火星"] {
            let started = std::time::Instant::now();
            let hits = db.search(q, None, 50).await.unwrap();
            let took = started.elapsed();
            println!("{q:?}: {} hits in {took:?}", hits.len());
            assert!(took < std::time::Duration::from_millis(50), "{q:?} took {took:?}");
            let started = std::time::Instant::now();
            let hits = db.search(q, Some("c:7"), 50).await.unwrap();
            let took = started.elapsed();
            println!("{q:?} in one chat: {} hits in {took:?}", hits.len());
            assert!(
                took < std::time::Duration::from_millis(50),
                "{q:?} in one chat took {took:?}"
            );
        }
    }

    /// A sender controls message text: characters whose lowercase has another length must not
    /// make a snippet cut a character in half, or lose its highlight.
    #[tokio::test]
    async fn snippets_never_cut_a_character() {
        let db = db().await;
        // Ⱥ grows by a byte when lowercased and K shrinks by two: same total length, shifted
        db.insert_message(&msg("alice", 1, "ȺȺ明天KȺ")).await.unwrap();
        db.insert_message(&msg("alice", 2, "Ⱥ明天KȺ")).await.unwrap();
        let hits = db.search("明天", None, 10).await.unwrap();
        assert_eq!(hit_times(&hits), vec![2, 1]);
        for h in hits {
            assert_eq!(marked(&h.snippet), ["明天"]);
        }
        assert!(db.search("İ", None, 10).await.is_ok());
        // ASCII case still doesn't matter
        db.insert_message(&msg("alice", 3, "Ünïcödé café")).await.unwrap();
        assert_eq!(marked(&db.search("é", None, 10).await.unwrap()[0].snippet), ["é"]);
    }

    #[test]
    fn snippets_mark_the_match_with_context_on_both_sides() {
        let long = format!("{}needle{}", "a".repeat(30), "b".repeat(30));
        let s = text_snippet(&long, "NEEDLE");
        assert_eq!(marked(&s), ["needle"]);
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        // at the very start and end: no dots
        assert_eq!(text_snippet("needle", "needle"), "\u{E000}needle\u{E001}");
        // no match: the beginning, unmarked
        assert_eq!(text_snippet("hello", "x"), "hello");
        // Ω shrinks and Ⱥ grows when lowercased: never a cut character, the match still marked
        assert_eq!(marked(&text_snippet("\u{2126}明天\u{023A}", "明天")), ["明天"]);
        assert_eq!(marked(&text_snippet("İstanbul 明天", "明天")), ["明天"]);
        assert_eq!(marked(&text_snippet("go 👍🏽 now", "👍🏽")), ["👍🏽"]);
    }

    #[tokio::test]
    async fn words_with_emoji_or_symbols_must_all_match() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi there")).await.unwrap();
        db.insert_message(&msg("alice", 2, "hi 🎉 everyone")).await.unwrap();
        db.insert_message(&msg("alice", 3, "ok :) bye")).await.unwrap();
        assert_eq!(hit_times(&db.search("hi 🎉", None, 10).await.unwrap()), vec![2]);
        assert_eq!(hit_times(&db.search(":)", None, 10).await.unwrap()), vec![3]);
    }

    #[tokio::test]
    async fn every_search_path_respects_the_chat() {
        let db = db().await;
        for (conv, t, body) in [("c:alice", 1, "unbelievable 明天"), ("c:bob", 2, "unbelievable 明天")] {
            db.insert_message(&in_conv(conv, msg(&conv[2..], t, body)))
                .await
                .unwrap();
        }
        for q in ["unbeliev", "believ", "明天"] {
            assert_eq!(
                hit_times(&db.search(q, Some("c:bob"), 10).await.unwrap()),
                vec![2],
                "{q}"
            );
        }
    }

    #[tokio::test]
    async fn messages_from_before_the_trigram_index_are_found_too() {
        // migration 0006 indexes what is already there ('rebuild'): simulate an older database
        let db = db().await;
        db.insert_message(&msg("alice", 1, "unbelievable")).await.unwrap();
        sqlx::query("INSERT INTO messages_trigram(messages_trigram) VALUES ('delete-all')")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.search("believ", None, 10).await.unwrap().is_empty());
        sqlx::query("INSERT INTO messages_trigram(messages_trigram) VALUES ('rebuild')")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(db.search("believ", None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn chat_events_are_kept_per_chat_in_time_order() {
        let db = db().await;
        db.add_chat_event("c:alice", 20, ChatEventKind::DecryptionFailed, "alice")
            .await
            .unwrap();
        db.add_chat_event("c:alice", 10, ChatEventKind::DecryptionFailed, "alice")
            .await
            .unwrap();
        db.add_chat_event("c:bob", 15, ChatEventKind::DecryptionFailed, "bob")
            .await
            .unwrap();
        let events = db.chat_events("c:alice", 0).await.unwrap();
        assert_eq!(events.iter().map(|e| e.at).collect::<Vec<_>>(), [10, 20]);
        assert_eq!(
            (events[0].kind, events[0].author.as_str()),
            (ChatEventKind::DecryptionFailed, "alice")
        );
        assert_eq!(
            db.chat_events("c:alice", 15).await.unwrap().len(),
            1,
            "only since the oldest shown"
        );
    }

    #[tokio::test]
    async fn a_chat_with_only_an_undecryptable_message_is_listed_and_moves_up() {
        let db = db().await;
        db.insert_message(&in_conv("c:bob", msg("bob", 50, "hi")))
            .await
            .unwrap();
        db.add_chat_event("c:alice", 10, ChatEventKind::DecryptionFailed, "alice")
            .await
            .unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (c.last_message_at, c.last_message_preview.as_str()),
            (10, UNDECRYPTABLE_PREVIEW)
        );
        assert!(c.last_message_undecryptable);
        // after a message of ours (deleted): the row is about the failure only
        db.insert_message(&Message {
            is_outgoing: true,
            status: MessageStatus::Read,
            ..in_conv("c:carol", msg("me", 5, "mine"))
        })
        .await
        .unwrap();
        db.apply(&Op::Delete { target: k("me", 5) }).await.unwrap();
        db.add_chat_event("c:carol", 9, ChatEventKind::DecryptionFailed, "carol")
            .await
            .unwrap();
        let c = conv(&db, "c:carol").await;
        assert!(c.last_message_undecryptable);
        assert!(!c.last_message_outgoing && !c.last_message_deleted && c.last_message_file.is_none());
        db.add_chat_event("c:bob", 60, ChatEventKind::DecryptionFailed, "bob")
            .await
            .unwrap();
        assert_eq!(db.conversations().await.unwrap()[0].id, "c:bob", "moved up");
        // a later message takes over the preview; our own session reset doesn't count
        db.insert_message(&in_conv("c:bob", msg("bob", 70, "back")))
            .await
            .unwrap();
        db.add_chat_event("c:bob", 80, ChatEventKind::SessionReset, "bob")
            .await
            .unwrap();
        let c = conv(&db, "c:bob").await;
        assert_eq!((c.last_message_at, c.last_message_preview.as_str()), (70, "back"));
        assert!(!c.last_message_undecryptable);
    }

    #[tokio::test]
    async fn placeholder_kinds_are_kept_and_named_in_the_chat_list() {
        let db = db().await;
        let sticker = Message {
            body: None,
            kind: MessageKind::Sticker {
                emoji: Some("🙂".into()),
            },
            ..msg("alice", 1, "")
        };
        db.insert_message(&sticker).await.unwrap();
        assert_eq!(db.message(&k("alice", 1)).await.unwrap().unwrap().kind, sticker.kind);
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (c.last_message_kind, c.unread_count),
            (sticker.kind, 1),
            "it counts as unread"
        );
    }

    #[tokio::test]
    async fn an_incoming_disappearing_message_starts_its_timer_when_read_and_then_goes() {
        let files = TempFiles::new("expiry");
        let db = db().await.with_files(files.0.clone());
        db.insert_message(&Message {
            expires_in: 60,
            ..msg("alice", 1, "secret plan")
        })
        .await
        .unwrap();
        db.insert_message(&msg("alice", 2, "stays")).await.unwrap();
        db.apply(&add(k("alice", 1), vec![new_attachment("image/jpeg", 3, false)], None))
            .await
            .unwrap();
        let id = db.attachments(&k("alice", 1)).await.unwrap()[0].id;
        db.start_download(id).await.unwrap();
        db.finish_download(id, Ok(b"jpg".to_vec())).await.unwrap();
        let path = db.attachment(id).await.unwrap().unwrap().path.unwrap();
        let far_future = i64::MAX / 2;
        assert!(
            db.delete_expired(far_future).await.unwrap().is_empty(),
            "unread: not started"
        );
        assert_eq!(db.message(&k("alice", 1)).await.unwrap().unwrap().expires_at, None);
        db.apply(&Op::SetStatus {
            targets: vec![k("alice", 1), k("alice", 2)],
            status: MessageStatus::Read,
        })
        .await
        .unwrap();
        let at = db.message(&k("alice", 1)).await.unwrap().unwrap().expires_at.unwrap();
        assert_eq!(
            db.message(&k("alice", 2)).await.unwrap().unwrap().expires_at,
            None,
            "no timer"
        );
        assert!(db.delete_expired(at - 1).await.unwrap().is_empty(), "not yet");
        assert_eq!(
            db.delete_expired(at).await.unwrap(),
            vec![("c:alice".to_string(), k("alice", 1))]
        );
        assert!(db.message(&k("alice", 1)).await.unwrap().is_none());
        assert!(
            db.search("secret", None, 10).await.unwrap().is_empty(),
            "out of the search index"
        );
        assert!(!std::path::Path::new(&path).exists(), "its file too");
        assert_eq!(conv(&db, "c:alice").await.last_message_preview, "stays");
    }

    #[tokio::test]
    async fn a_timer_change_sets_the_chat_timer_and_shows_in_the_chat() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        let applied = db
            .apply(&Op::SetTimer {
                conversation_id: "c:alice".into(),
                author: "alice".into(),
                seconds: 604_800,
                at: 5,
            })
            .await
            .unwrap();
        assert_eq!(
            applied,
            vec![Applied::ChatEvent {
                conversation_id: "c:alice".into()
            }]
        );
        assert_eq!(db.expire_timer("c:alice").await.unwrap(), 604_800);
        assert_eq!(conv(&db, "c:alice").await.expire_timer, 604_800);
        let event = &db.chat_events("c:alice", 0).await.unwrap()[0];
        assert_eq!((event.kind, event.value), (ChatEventKind::TimerChanged, Some(604_800)));
        // an older message doesn't undo it; a newer one carries the chat's current timer
        db.insert_message(&msg("alice", 3, "old")).await.unwrap();
        assert_eq!(db.expire_timer("c:alice").await.unwrap(), 604_800);
        db.insert_message(&Message {
            expires_in: 3600,
            ..msg("alice", 9, "new")
        })
        .await
        .unwrap();
        assert_eq!(db.expire_timer("c:alice").await.unwrap(), 3600);
    }
}
