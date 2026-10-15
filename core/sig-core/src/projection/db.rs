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
}

/// Encrypted SQLite store the UI reads from. Separate from presage's database because both use
/// `sqlx::migrate!`, which would collide on `_sqlx_migrations`.
///
/// The pool has one connection, so a method must never call another `ProjectionDb` method while
/// it holds a transaction: multi-step work goes through `&mut SqliteConnection` helpers.
#[derive(Clone)]
pub struct ProjectionDb {
    pool: SqlitePool,
}

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
        Ok(Self { pool })
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
        let kind = kind_of(&m.conversation_id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(&m.conversation_id)
            .bind(kind_to_i64(kind))
            .execute(&mut *tx)
            .await?;
        let quote = m.quote.as_ref();
        let (body, quote_text) = if m.deleted {
            (None, None)
        } else {
            (m.body.as_ref(), quote.and_then(|q| q.text.as_ref()))
        };
        let res = sqlx::query(
            "INSERT OR IGNORE INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
               status, edited, deleted, quote_author, quote_sent_at, quote_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        .execute(&mut *tx)
        .await?;
        let inserted = res.rows_affected() == 1;
        if inserted {
            apply_pending(&mut tx, &m.key()).await?;
            refresh_summary(&mut tx, &m.conversation_id).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Conversations that have messages, most recent first. Contacts known only by name (synced
    /// from the phone) aren't listed until a message arrives.
    pub async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error> {
        let rows =
            sqlx::query("SELECT * FROM conversations WHERE last_message_at > 0 ORDER BY last_message_at DESC, id")
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

    /// Full-text search over message bodies, newest first. The query is matched as a phrase, so
    /// FTS5 operators in the input are plain text. Case and diacritics are folded; emoji, CJK
    /// substrings and word prefixes don't match yet (search UI ticket).
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>, sqlx::Error> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let rows = sqlx::query(
            "SELECT m.* FROM messages_fts f JOIN messages m ON m.id = f.rowid
             WHERE messages_fts MATCH ?1 AND m.deleted = 0 ORDER BY m.sent_at DESC, m.author DESC LIMIT ?2",
        )
        .bind(phrase)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_message).collect()
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
                forget_deleted_text(&mut tx, target).await?;
                let m = finish_update(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                Ok(changed(m))
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
                    out.extend(changed(finish_update(&mut tx, target, res.rows_affected()).await?));
                }
                tx.commit().await?;
                Ok(out)
            }
        }
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
async fn forget_deleted_text(conn: &mut SqliteConnection, key: &MessageKey) -> Result<(), sqlx::Error> {
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
    sqlx::query(
        "UPDATE conversations SET
           last_message_at = COALESCE((SELECT MAX(sent_at) FROM messages WHERE conversation_id = ?1), 0),
           last_message_preview = COALESCE((SELECT body FROM messages WHERE conversation_id = ?1
                                            ORDER BY sent_at DESC, author DESC LIMIT 1), ''),
           unread_count = (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1
                           AND is_outgoing = 0 AND deleted = 0 AND status < 3)
         WHERE id = ?1",
    )
    .bind(conversation_id)
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
    Ok(Conversation {
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
    })
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
        }
    }

    fn in_conv(conv: &str, m: Message) -> Message {
        Message {
            conversation_id: conv.into(),
            ..m
        }
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
        assert_eq!(times(&db.search("secret", 10).await.unwrap()), vec![2]);
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
        assert_eq!(db.search("hi", 10).await.unwrap().len(), 1);
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
        assert_eq!(times(&db.search("coffee", 2).await.unwrap()), vec![3, 2]);
        assert_eq!(times(&db.search("coffee", 10).await.unwrap()), vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn search_folds_case_and_diacritics() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "Café LUNCH")).await.unwrap();
        assert_eq!(db.search("cafe", 10).await.unwrap().len(), 1);
        assert_eq!(db.search("lunch", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_treats_fts_syntax_as_text() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "say \"hi\" (now) AND NOT -x*"))
            .await
            .unwrap();
        db.insert_message(&msg("alice", 2, "plain words")).await.unwrap();
        assert_eq!(times(&db.search("\"hi\"", 10).await.unwrap()), vec![1]);
        assert_eq!(times(&db.search("(now)", 10).await.unwrap()), vec![1]);
        assert_eq!(times(&db.search("AND NOT", 10).await.unwrap()), vec![1]);
        assert!(db.search("-x*", 10).await.is_ok());
        assert!(db.search("\"", 10).await.unwrap().is_empty());
        assert!(db.search("   ", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts_follows_body_updates_and_row_deletes() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "old words")).await.unwrap();
        sqlx::query("UPDATE messages SET body = 'new words' WHERE author = 'alice' AND sent_at = 1")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.search("old", 10).await.unwrap().is_empty());
        assert_eq!(db.search("new", 10).await.unwrap().len(), 1);
        sqlx::query("DELETE FROM messages").execute(&db.pool).await.unwrap();
        db.insert_message(&msg("bob", 2, "fresh")).await.unwrap(); // may reuse the deleted rowid
        assert!(db.search("new", 10).await.unwrap().is_empty());
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
        assert_eq!(db.search("second", 5).await.unwrap().len(), 1);
        assert!(db.search("typo", 5).await.unwrap().is_empty());
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
        assert!(db.search("secret", 5).await.unwrap().is_empty());
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
        assert!(db.search("secret", 5).await.unwrap().is_empty());
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
}
