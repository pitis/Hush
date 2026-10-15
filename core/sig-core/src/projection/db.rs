use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;

use super::model::*;

/// Encrypted SQLite store the UI reads from. Separate from presage's database because both use
/// `sqlx::migrate!`, which would collide on `_sqlx_migrations`.
#[derive(Clone)]
pub struct ProjectionDb {
    pool: SqlitePool,
}

impl ProjectionDb {
    /// Opens (creating if needed) and migrates the database. `key` is the 64-char hex SQLCipher
    /// key; `None` opens an unencrypted database (tests).
    pub async fn open(url: &str, key: Option<&str>) -> Result<Self, sqlx::Error> {
        let mut opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true);
        if let Some(k) = key {
            if k.len() != 64 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(sqlx::Error::Configuration(
                    "database key must be 64 hex characters".into(),
                ));
            }
            opts = opts.pragma("key", format!("\"x'{k}'\""));
        }
        // One connection: in-memory databases are per connection, and the app's write volume is tiny.
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn upsert_conversation_title(
        &self,
        id: &str,
        kind: ConversationKind,
        title: &str,
    ) -> Result<(), sqlx::Error> {
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

    /// Inserts a message and updates its conversation's preview and unread count. Returns false
    /// (and changes nothing) if a message with the same `(author, sent_at)` already exists.
    pub async fn insert_message(&self, m: &Message) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let kind = if m.conversation_id.starts_with("g:") {
            ConversationKind::Group
        } else {
            ConversationKind::Direct
        };
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(&m.conversation_id)
            .bind(kind_to_i64(kind))
            .execute(&mut *tx)
            .await?;
        let quote = m.quote.as_ref();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
               status, edited, deleted, quote_author, quote_sent_at, quote_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(&m.conversation_id)
        .bind(&m.author)
        .bind(m.sent_at)
        .bind(m.received_at)
        .bind(&m.body)
        .bind(m.is_outgoing)
        .bind(m.status.to_i64())
        .bind(m.edited)
        .bind(m.deleted)
        .bind(quote.map(|q| q.author.clone()))
        .bind(quote.map(|q| q.sent_at))
        .bind(quote.and_then(|q| q.text.clone()))
        .execute(&mut *tx)
        .await?;
        let inserted = res.rows_affected() == 1;
        if inserted {
            sqlx::query(
                "UPDATE conversations SET
                   last_message_preview = CASE WHEN ?2 >= last_message_at THEN COALESCE(?3, '') ELSE last_message_preview END,
                   last_message_at = MAX(last_message_at, ?2),
                   unread_count = unread_count + ?4
                 WHERE id = ?1",
            )
            .bind(&m.conversation_id)
            .bind(m.sent_at)
            .bind(&m.body)
            .bind(i64::from(!m.is_outgoing))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// All conversations, most recent first.
    pub async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error> {
        let rows = sqlx::query("SELECT * FROM conversations ORDER BY last_message_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| Conversation {
                id: r.get("id"),
                kind: if r.get::<i64, _>("kind") == 1 {
                    ConversationKind::Group
                } else {
                    ConversationKind::Direct
                },
                title: r.get("title"),
                last_message_at: r.get("last_message_at"),
                last_message_preview: r.get("last_message_preview"),
                unread_count: u32::try_from(r.get::<i64, _>("unread_count")).unwrap_or(0),
            })
            .collect())
    }

    /// Up to `limit` messages sent before `before` (or the newest ones), oldest first.
    pub async fn messages(
        &self,
        conversation_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let mut rows = sqlx::query(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND sent_at < ?2
             ORDER BY sent_at DESC LIMIT ?3",
        )
        .bind(conversation_id)
        .bind(before.unwrap_or(i64::MAX))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.reverse();
        Ok(rows.iter().map(row_to_message).collect())
    }

    /// Full-text search over message bodies, newest first. The query is matched as a phrase, so
    /// FTS5 operators in the input are treated as plain text.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>, sqlx::Error> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let rows = sqlx::query(
            "SELECT m.* FROM messages_fts f JOIN messages m ON m.id = f.rowid
             WHERE messages_fts MATCH ?1 AND m.deleted = 0 ORDER BY m.sent_at DESC LIMIT ?2",
        )
        .bind(phrase)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_message).collect())
    }
}

fn kind_to_i64(kind: ConversationKind) -> i64 {
    match kind {
        ConversationKind::Direct => 0,
        ConversationKind::Group => 1,
    }
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
        quote: quote_sent_at.map(|sent_at| Quote {
            author: r.get("quote_author"),
            sent_at,
            text: r.get("quote_text"),
        }),
    }
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

    async fn alice(db: &ProjectionDb) {
        db.upsert_conversation_title("c:alice", ConversationKind::Direct, "Alice")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn insert_and_page_messages() {
        let db = db().await;
        alice(&db).await;
        for t in 1..=5 {
            db.insert_message(&msg("alice", t, &format!("m{t}"))).await.unwrap();
        }
        let page = db.messages("c:alice", Some(5), 2).await.unwrap();
        assert_eq!(page.iter().map(|m| m.sent_at).collect::<Vec<_>>(), vec![3, 4]);
        let newest = db.messages("c:alice", None, 2).await.unwrap();
        assert_eq!(newest.iter().map(|m| m.sent_at).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[tokio::test]
    async fn message_round_trips_all_fields() {
        let db = db().await;
        alice(&db).await;
        let m = Message {
            is_outgoing: true,
            status: MessageStatus::Read,
            quote: Some(Quote {
                author: "bob".into(),
                sent_at: 9,
                text: Some("q".into()),
            }),
            ..msg("me", 10, "hi")
        };
        db.insert_message(&m).await.unwrap();
        assert_eq!(db.messages("c:alice", None, 10).await.unwrap(), vec![m]);
    }

    #[tokio::test]
    async fn duplicate_message_is_ignored_and_not_double_counted() {
        let db = db().await;
        alice(&db).await;
        assert!(db.insert_message(&msg("alice", 10, "hi")).await.unwrap());
        assert!(!db.insert_message(&msg("alice", 10, "hi")).await.unwrap());
        let convs = db.conversations().await.unwrap();
        assert_eq!(convs[0].unread_count, 1);
        assert_eq!(convs[0].last_message_preview, "hi");
        assert_eq!(convs[0].last_message_at, 10);
    }

    #[tokio::test]
    async fn older_message_does_not_replace_preview() {
        let db = db().await;
        alice(&db).await;
        db.insert_message(&msg("alice", 10, "new")).await.unwrap();
        db.insert_message(&msg("alice", 5, "old")).await.unwrap();
        let c = &db.conversations().await.unwrap()[0];
        assert_eq!((c.last_message_at, c.last_message_preview.as_str()), (10, "new"));
    }

    #[tokio::test]
    async fn conversations_ordered_by_recency() {
        let db = db().await;
        alice(&db).await;
        db.upsert_conversation_title("c:bob", ConversationKind::Direct, "Bob")
            .await
            .unwrap();
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&Message {
            conversation_id: "c:bob".into(),
            ..msg("bob", 2, "b")
        })
        .await
        .unwrap();
        let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c:bob", "c:alice"]);
    }

    #[tokio::test]
    async fn message_for_unknown_conversation_creates_it() {
        let db = db().await;
        db.insert_message(&Message {
            conversation_id: "g:abcd".into(),
            ..msg("bob", 1, "x")
        })
        .await
        .unwrap();
        let c = &db.conversations().await.unwrap()[0];
        assert_eq!((c.id.as_str(), c.kind), ("g:abcd", ConversationKind::Group));
    }

    #[tokio::test]
    async fn empty_title_does_not_overwrite_known_title() {
        let db = db().await;
        alice(&db).await;
        db.upsert_conversation_title("c:alice", ConversationKind::Direct, "")
            .await
            .unwrap();
        assert_eq!(db.conversations().await.unwrap()[0].title, "Alice");
    }

    #[tokio::test]
    async fn fts5_search_finds_words() {
        let db = db().await;
        alice(&db).await;
        db.insert_message(&msg("alice", 1, "lunch tomorrow at noon"))
            .await
            .unwrap();
        db.insert_message(&msg("alice", 2, "unrelated")).await.unwrap();
        let hits = db.search("tomorrow", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sent_at, 1);
    }

    #[tokio::test]
    async fn search_survives_fts_syntax_in_input() {
        let db = db().await;
        alice(&db).await;
        db.insert_message(&msg("alice", 1, "say \"hi\" (now) AND NOT -x*"))
            .await
            .unwrap();
        for q in ["\"hi\"", "(now", "AND NOT", "-x*", "", "\""] {
            db.search(q, 10)
                .await
                .unwrap_or_else(|e| panic!("query {q:?} failed: {e}"));
        }
        assert_eq!(db.search("(now)", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn outgoing_message_does_not_increment_unread() {
        let db = db().await;
        alice(&db).await;
        db.insert_message(&Message {
            is_outgoing: true,
            ..msg("me", 1, "yo")
        })
        .await
        .unwrap();
        assert_eq!(db.conversations().await.unwrap()[0].unread_count, 0);
    }

    #[tokio::test]
    async fn encrypted_file_reopens_with_key_and_rejects_wrong_key() {
        let path = std::env::temp_dir().join(format!("semaphore-proj-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}", path.display());
        let key = "ab".repeat(32);
        {
            let db = ProjectionDb::open(&url, Some(&key)).await.unwrap();
            alice(&db).await;
        }
        let db = ProjectionDb::open(&url, Some(&key)).await.unwrap();
        assert_eq!(db.conversations().await.unwrap().len(), 1);
        assert!(ProjectionDb::open(&url, Some(&"cd".repeat(32))).await.is_err());
        let _ = std::fs::remove_file(&path);
    }
}
