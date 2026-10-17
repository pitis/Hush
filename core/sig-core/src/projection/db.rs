use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

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
    /// Chats' flags changed, or a chat went (blocks and request answers from our phone).
    ConversationsChanged,
    /// A story came (or went).
    StoriesChanged,
}

/// How long a story shows.
pub const STORY_LIFETIME_MS: i64 = 24 * 3600 * 1000;

/// SQL: the chat `c` is a message request (see `Conversation::message_request`). A macro so
/// queries stay static strings (`concat!`).
macro_rules! is_request {
    () => {
        "(EXISTS (SELECT 1 FROM storage_sync WHERE version > 0)
           AND NOT c.whitelisted AND NOT c.blocked AND NOT c.phone_contact
           AND NOT EXISTS (SELECT 1 FROM messages o WHERE o.conversation_id = c.id AND o.is_outgoing = 1))"
    };
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
    /// Counts pin changes (`set_account_dirty(true)`), so the sync clears the account's mark
    /// only if nothing changed while it wrote; the lock makes the count and the mark move together.
    account_changes: Arc<tokio::sync::Mutex<u64>>,
}

/// Signal's limit for one attachment: larger ones are neither sent nor downloaded.
pub const MAX_ATTACHMENT: u64 = 100 * 1024 * 1024;

/// A message whose disappearing timer ran out (see `delete_expired`).
#[derive(Debug, Clone, PartialEq)]
pub struct Expired {
    pub conversation_id: String,
    pub key: MessageKey,
}

/// The chat list's preview when the latest thing in a chat is a message that couldn't be
/// decrypted.
pub const UNDECRYPTABLE_PREVIEW: &str = "A message couldn't be decrypted";

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
#[derive(Clone, PartialEq, Eq)]
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

/// The key decrypts the whole archive: it never goes to a log, not even through `{:?}`.
impl std::fmt::Debug for HistoryImportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryImportState")
            .field("stage", &self.stage)
            .field("key", &"<redacted>")
            .field("aci", &self.aci)
            .field("cdn", &self.cdn)
            .field("cdn_key", &self.cdn_key)
            .field("bytes_done", &self.bytes_done)
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

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
        Ok(Self {
            pool,
            files: None,
            account_changes: Default::default(),
        })
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

    /// Replaces a group's members (ACIs) with `acis`.
    pub async fn set_group_members(&self, conversation_id: &str, acis: &[String]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM group_members WHERE conversation_id = ?1")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        for aci in acis {
            sqlx::query("INSERT OR IGNORE INTO group_members(conversation_id, aci) VALUES (?1, ?2)")
                .bind(conversation_id)
                .bind(aci)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// The poll in message `key` as its votes stand; None if it isn't (or no longer) here. A
    /// single-choice poll counts each voter's first pick; picks it doesn't have don't count.
    pub async fn poll(&self, key: &MessageKey) -> Result<Option<Poll>, sqlx::Error> {
        let Some(row) =
            sqlx::query("SELECT options, allow_multiple, ended FROM polls WHERE author = ?1 AND sent_at = ?2")
                .bind(&key.author)
                .bind(key.sent_at)
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(None);
        };
        let texts: Vec<String> = serde_json::from_str(&row.try_get::<String, _>("options")?)
            .map_err(|e| decode_err(format!("poll options: {e}")))?;
        if texts.is_empty() {
            return Ok(None); // only its end is here
        }
        let allow_multiple: bool = row.try_get("allow_multiple")?;
        let mut options: Vec<PollOption> = texts
            .into_iter()
            .map(|text| PollOption { text, voters: vec![] })
            .collect();
        let votes = sqlx::query(
            "SELECT voter, options FROM poll_votes WHERE target_author = ?1 AND target_sent_at = ?2 ORDER BY voter",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_all(&self.pool)
        .await?;
        for v in votes {
            let voter: String = v.try_get("voter")?;
            let picks: Vec<u32> = serde_json::from_str(&v.try_get::<String, _>("options")?).unwrap_or_default();
            let counted = if allow_multiple {
                &picks[..]
            } else {
                &picks[..picks.len().min(1)]
            };
            for &i in counted {
                if let Some(o) = options.get_mut(i as usize) {
                    if !o.voters.contains(&voter) {
                        o.voters.push(voter.clone());
                    }
                }
            }
        }
        Ok(Some(Poll {
            options,
            allow_multiple,
            ended: row.try_get("ended")?,
        }))
    }

    /// The `vote_count` of `voter`'s latest vote on the poll `key` (0: none yet).
    pub async fn my_poll_vote_count(&self, key: &MessageKey, voter: &str) -> Result<u32, sqlx::Error> {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT vote_count FROM poll_votes WHERE target_author = ?1 AND target_sent_at = ?2 AND voter = ?3",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .bind(voter)
        .fetch_optional(&self.pool)
        .await?;
        Ok(n.and_then(|n| u32::try_from(n).ok()).unwrap_or(0))
    }

    /// The stories of the last day from chats that aren't blocked or message requests (ours too),
    /// oldest first, each with its file when it's a photo or video.
    pub async fn stories(&self, now_ms: i64) -> Result<Vec<Story>, sqlx::Error> {
        let rows = sqlx::query(concat!(
            "SELECT s.* FROM stories s LEFT JOIN conversations c ON c.id = s.conversation_id
             WHERE s.sent_at > ?1 AND (s.outgoing OR (c.id IS NOT NULL AND NOT c.blocked AND NOT ",
            is_request!(),
            ")) ORDER BY s.sent_at, s.author"
        ))
        .bind(now_ms - STORY_LIFETIME_MS)
        .fetch_all(&self.pool)
        .await?;
        let mut stories = vec![];
        for r in rows {
            let author: String = r.try_get("author")?;
            let sent_at: i64 = r.try_get("sent_at")?;
            let text = match r.try_get::<Option<String>, _>("text")? {
                Some(text) => Some(StoryText {
                    text,
                    style: u32::try_from(r.try_get::<i64, _>("text_style")?).unwrap_or(0),
                    foreground: r
                        .try_get::<Option<i64>, _>("text_foreground")?
                        .and_then(|c| u32::try_from(c).ok()),
                    text_background: r
                        .try_get::<Option<i64>, _>("text_background")?
                        .and_then(|c| u32::try_from(c).ok()),
                    background: serde_json::from_str(&r.try_get::<String, _>("background")?).unwrap_or_default(),
                    angle: u32::try_from(r.try_get::<i64, _>("angle")?).unwrap_or(0),
                    link: r.try_get("link")?,
                }),
                None => None,
            };
            let file_id: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 AND position = 0",
            )
            .bind(&author)
            .bind(sent_at)
            .fetch_optional(&self.pool)
            .await?;
            let file = match file_id {
                Some(id) => self.attachment(id).await?,
                None => None,
            };
            stories.push(Story {
                conversation_id: r.try_get("conversation_id")?,
                author,
                sent_at,
                text,
                file,
                viewed: r.try_get("viewed")?,
            });
        }
        Ok(stories)
    }

    /// Removes stories older than a day, with their files. True if any went.
    pub async fn expire_stories(&self, now_ms: i64) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let old: Vec<(String, i64)> =
            sqlx::query_as("DELETE FROM stories WHERE sent_at <= ?1 RETURNING author, sent_at")
                .bind(now_ms - STORY_LIFETIME_MS)
                .fetch_all(&mut *tx)
                .await?;
        let mut files = vec![];
        for (author, sent_at) in &old {
            files.extend(forget_story_files(&mut tx, author, *sent_at).await?);
        }
        tx.commit().await?;
        self.remove_files(&files);
        Ok(!old.is_empty())
    }

    /// Marks a story seen here.
    pub async fn story_viewed(&self, key: &MessageKey) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE stories SET viewed = 1 WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A group's members (ACIs), as last synced from presage's group store.
    pub async fn group_members(&self, conversation_id: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT aci FROM group_members WHERE conversation_id = ?1 ORDER BY aci")
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await
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

    /// How many chats can be pinned (the phone's limit).
    pub const MAX_PINNED: u32 = 4;

    /// Archiving also unpins (as Desktop): an archived chat holds no pin slot and comes back
    /// unpinned.
    pub async fn set_archived(&self, id: &str, archived: bool) -> Result<(), sqlx::Error> {
        let pinned: Option<Option<i64>> = sqlx::query_scalar("SELECT pinned_order FROM conversations WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        sqlx::query(
            "UPDATE conversations SET archived = ?2,
               pinned_order = CASE WHEN ?2 THEN NULL ELSE pinned_order END, storage_dirty = 1
             WHERE id = ?1",
        )
        .bind(id)
        .bind(archived)
        .execute(&self.pool)
        .await?;
        if archived && matches!(pinned, Some(Some(_))) {
            self.set_account_dirty(true).await?; // the pin went with it
        }
        Ok(())
    }

    /// Pins a chat after the ones pinned already (and takes it out of the archive), or unpins it.
    /// False, and nothing changed, if pinning would go past `MAX_PINNED`.
    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<bool, sqlx::Error> {
        if !pinned {
            sqlx::query("UPDATE conversations SET pinned_order = NULL WHERE id = ?1")
                .bind(id)
                .execute(&self.pool)
                .await?;
            self.set_account_dirty(true).await?;
            return Ok(true);
        }
        // (archived may flip too, so the chat's own record is dirty as well)
        let res = sqlx::query(
            "UPDATE conversations SET archived = 0, storage_dirty = 1,
               pinned_order = COALESCE(pinned_order,
                 (SELECT COALESCE(MAX(pinned_order), 0) + 1 FROM conversations))
             WHERE id = ?1 AND (pinned_order IS NOT NULL
               OR (SELECT COUNT(*) FROM conversations WHERE pinned_order IS NOT NULL) < ?2)",
        )
        .bind(id)
        .bind(i64::from(Self::MAX_PINNED))
        .execute(&self.pool)
        .await?;
        let done = res.rows_affected() == 1;
        if done {
            self.set_account_dirty(true).await?;
        }
        Ok(done)
    }

    /// `until`: ms since the epoch; 0 unmutes; `i64::MAX` mutes for good.
    pub async fn set_mute_until(&self, id: &str, until: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE conversations SET mute_until = ?2, storage_dirty = 1 WHERE id = ?1")
            .bind(id)
            .bind(until)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_marked_unread(&self, id: &str, marked: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE conversations SET marked_unread = ?2, storage_dirty = 1 WHERE id = ?1")
            .bind(id)
            .bind(marked)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Blocking and message requests ----

    /// Blocks a chat (profile sharing goes off with it, as on Signal), for the phone to learn.
    pub async fn block(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE conversations SET blocked = 1, whitelisted = 0, storage_dirty = 1 WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Accepts a chat's message request, or unblocks it (the same on Signal), for the phone to
    /// learn. True if it was blocked.
    pub async fn accept(&self, id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let was: Option<bool> = sqlx::query_scalar("SELECT blocked FROM conversations WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query("UPDATE conversations SET blocked = 0, whitelisted = 1, storage_dirty = 1 WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(was.unwrap_or(false))
    }

    /// Whether the chat is blocked (false for a chat we don't have).
    pub async fn is_blocked(&self, id: &str) -> Result<bool, sqlx::Error> {
        let b: Option<bool> = sqlx::query_scalar("SELECT blocked FROM conversations WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(b.unwrap_or(false))
    }

    /// Every blocked chat, listed or not, by id.
    pub async fn blocked_chats(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT id FROM conversations WHERE blocked = 1 ORDER BY id")
            .fetch_all(&self.pool)
            .await
    }

    /// Listed, accepted chats whose picture wasn't looked for in the last day, at most
    /// `limit`, the most recent first.
    pub async fn avatars_to_fetch(&self, now_ms: i64, limit: u32) -> Result<Vec<String>, sqlx::Error> {
        // not a blocked chat's or a message request's: a stranger chooses that picture (as
        // Signal, which hides it until accepted)
        sqlx::query_scalar(concat!(
            "SELECT c.id FROM conversations c WHERE c.last_message_at > 0 AND c.avatar_checked_at <= ?1
               AND NOT c.blocked AND NOT ",
            is_request!(),
            " ORDER BY c.last_message_at DESC, c.id LIMIT ?2"
        ))
        .bind(now_ms - 24 * 3600 * 1000)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
    }

    /// Keeps a chat's fetched picture as a file beside the attachments (None: it has none), as
    /// looked for at `now_ms`. A new picture gets a new file (the app caches pictures by path)
    /// and the old one goes. True if the picture changed.
    pub async fn store_avatar(&self, id: &str, bytes: Option<&[u8]>, now_ms: i64) -> Result<bool, sqlx::Error> {
        let dir = self
            .files
            .clone()
            .ok_or_else(|| sqlx::Error::Configuration("no attachments folder".into()))?;
        let old: Option<String> = sqlx::query_scalar("SELECT avatar FROM conversations WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .flatten();
        let same = match (&old, bytes) {
            (Some(path), Some(bytes)) => std::fs::read(path).is_ok_and(|b| b == bytes),
            (None, None) => true,
            _ => false,
        };
        if same {
            self.set_avatar_checked(id, now_ms).await?;
            return Ok(false);
        }
        let path = match bytes {
            Some(bytes) => {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("avatar-{}-{now_ms}", id.replace(':', "-")));
                std::fs::write(&path, bytes)?;
                Some(path.to_string_lossy().into_owned())
            }
            None => None,
        };
        self.set_avatar(id, path.as_deref(), now_ms).await?;
        if let Some(old) = old {
            let _ = std::fs::remove_file(old);
        }
        Ok(true)
    }

    /// A chat's picture was looked for (and not found or not reachable) at `now_ms`.
    pub async fn set_avatar_checked(&self, id: &str, now_ms: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE conversations SET avatar_checked_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(now_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Stores a chat's picture (None: it has none) as looked for at `now_ms`. True if the
    /// picture changed.
    pub async fn set_avatar(&self, id: &str, path: Option<&str>, now_ms: i64) -> Result<bool, sqlx::Error> {
        let changed = sqlx::query("UPDATE conversations SET avatar = ?2 WHERE id = ?1 AND avatar IS NOT ?2")
            .bind(id)
            .bind(path)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1;
        sqlx::query("UPDATE conversations SET avatar_checked_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(now_ms)
            .execute(&self.pool)
            .await?;
        Ok(changed)
    }

    /// Sets our bubbles' colour in a chat (None: the default). False if there's no such chat.
    pub async fn set_chat_color(&self, conversation_id: &str, color: Option<&ChatColor>) -> Result<bool, sqlx::Error> {
        let done = sqlx::query("UPDATE conversations SET chat_color = ?2 WHERE id = ?1")
            .bind(conversation_id)
            .bind(color.map(ChatColor::to_row))
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Sets a chat's wallpaper (None: the default). False if there's no such chat.
    pub async fn set_wallpaper(
        &self,
        conversation_id: &str,
        wallpaper: Option<&ChatColor>,
    ) -> Result<bool, sqlx::Error> {
        let done = sqlx::query("UPDATE conversations SET wallpaper = ?2 WHERE id = ?1")
            .bind(conversation_id)
            .bind(wallpaper.map(ChatColor::to_row))
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Stores what the phone calls a contact (preferred to their profile's name) and the note
    /// about them. True if either changed.
    pub async fn set_contact_details(
        &self,
        id: &str,
        name: Option<&str>,
        note: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(id)
            .bind(kind_to_i64(kind))
            .execute(&self.pool)
            .await?;
        let res = sqlx::query(
            "UPDATE conversations SET storage_name = ?2, note = ?3
             WHERE id = ?1 AND (storage_name IS NOT ?2 OR note IS NOT ?3)",
        )
        .bind(id)
        .bind(name)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Stores a contact's username (None: it has none). True if it changed.
    pub async fn set_username(&self, id: &str, username: Option<&str>) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(id)
            .bind(kind_to_i64(kind))
            .execute(&self.pool)
            .await?;
        let res = sqlx::query("UPDATE conversations SET username = ?2 WHERE id = ?1 AND username IS NOT ?2")
            .bind(id)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// A chat found by `username`: listed now (`at`) so it can be opened and written in, unless
    /// it already has messages.
    pub async fn open_found(&self, id: &str, username: &str, at: i64) -> Result<(), sqlx::Error> {
        self.set_username(id, Some(username)).await?;
        sqlx::query("UPDATE conversations SET last_message_at = ?2 WHERE id = ?1 AND last_message_at = 0")
            .bind(id)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether the contact is in the phone's address book (its storage record carries system
    /// names), which makes it never a request. The chat is created if needed.
    /// True if it changed.
    pub async fn set_phone_contact(&self, id: &str, on: bool) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        let res = sqlx::query(
            "INSERT INTO conversations(id, kind, phone_contact) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET phone_contact = excluded.phone_contact
               WHERE phone_contact != excluded.phone_contact",
        )
        .bind(id)
        .bind(kind_to_i64(kind))
        .bind(on)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Deletes a chat's contents here: messages, their files, reactions, edits, events and held
    /// receipts; its request is no longer accepted (`whitelisted` off, for the phone to learn).
    /// The row and its other flags stay, unlisted until a new message. presage's copy of the
    /// thread is queued (`presage_forget_threads`) in the same transaction.
    pub async fn delete_chat(&self, id: &str) -> Result<(), sqlx::Error> {
        let files = self.delete_chat_in(id, true).await?;
        self.remove_files(&files);
        Ok(())
    }

    async fn delete_chat_in(&self, id: &str, dirty: bool) -> Result<Vec<String>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let keys: Vec<(String, i64)> =
            sqlx::query_as("SELECT author, sent_at FROM messages WHERE conversation_id = ?1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let mut files = vec![];
        for (author, sent_at) in keys {
            let key = MessageKey { author, sent_at };
            files.extend(forget_deleted_text(&mut tx, &key).await?);
            for sql in [
                "DELETE FROM unsent_receipts WHERE author = ?1 AND sent_at = ?2",
                "DELETE FROM pending_deletes WHERE author = ?1 AND sent_at = ?2",
                "DELETE FROM pending_status WHERE author = ?1 AND sent_at = ?2",
            ] {
                sqlx::query(sql)
                    .bind(&key.author)
                    .bind(key.sent_at)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM message_revisions WHERE author = ?1 AND original_sent_at = ?2")
                .bind(&key.author)
                .bind(key.sent_at)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM chat_events WHERE conversation_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let stories: Vec<(String, i64)> =
            sqlx::query_as("DELETE FROM stories WHERE conversation_id = ?1 RETURNING author, sent_at")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for (author, sent_at) in stories {
            files.extend(forget_story_files(&mut tx, &author, sent_at).await?);
        }
        sqlx::query("INSERT OR IGNORE INTO presage_forget_threads(conversation_id) VALUES (?1)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // unpinned too (as Desktop): an unlisted chat would hold a pin slot no one can free
        let unpinned =
            sqlx::query("UPDATE conversations SET pinned_order = NULL WHERE id = ?1 AND pinned_order IS NOT NULL")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                == 1;
        sqlx::query(
            "UPDATE conversations SET whitelisted = 0, marked_unread = 0,
               storage_dirty = CASE WHEN ?2 THEN 1 ELSE storage_dirty END WHERE id = ?1",
        )
        .bind(id)
        .bind(dirty)
        .execute(&mut *tx)
        .await?;
        refresh_summary(&mut tx, id).await?;
        tx.commit().await?;
        if unpinned && dirty {
            self.set_account_dirty(true).await?;
        }
        Ok(files)
    }

    /// presage threads (deleted chats) still to clear there.
    pub async fn presage_forget_threads(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT conversation_id FROM presage_forget_threads ORDER BY conversation_id")
            .fetch_all(&self.pool)
            .await
    }

    /// One queued presage thread is cleared.
    pub async fn presage_thread_forgotten(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM presage_forget_threads WHERE conversation_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// presage keeps a copy of a message we didn't take (from a blocked sender): queued for
    /// deletion there, like an expired message's.
    pub async fn queue_presage_forget(&self, conversation_id: &str, sent_at: i64) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO presage_forget(conversation_id, sent_at) VALUES (?1, ?2)")
            .bind(conversation_id)
            .bind(sent_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The chat a request response from our phone names, if we have it (groups by identifier).
    async fn request_chat(&self, thread: &RequestThread) -> Result<Option<String>, sqlx::Error> {
        Ok(match thread {
            RequestThread::Aci(aci) => Some(format!("c:{aci}")),
            RequestThread::Group(identifier) => self.group_by_identifier(identifier).await?,
        })
    }

    /// The group chat with this identifier (how typing messages and block lists name groups).
    pub async fn group_by_identifier(&self, identifier: &[u8]) -> Result<Option<String>, sqlx::Error> {
        let groups: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations WHERE kind = 1")
            .fetch_all(&self.pool)
            .await?;
        Ok(groups
            .into_iter()
            .find(|g| super::ids::group_identifier(g).as_deref() == Some(identifier)))
    }

    /// Sets a chat's blocked state as our phone reports it (not written back): blocked means not
    /// accepted, unblocked means accepted, as on Signal. The chat is created if needed. True if
    /// anything changed.
    async fn apply_phone_block(&self, id: &str, blocked: bool) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(id)
            .bind(kind_to_i64(kind))
            .execute(&mut *tx)
            .await?;
        let res = sqlx::query(
            "UPDATE conversations SET blocked = ?2, whitelisted = NOT ?2
             WHERE id = ?1 AND (blocked != ?2 OR whitelisted != NOT ?2)",
        )
        .bind(id)
        .bind(blocked)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    pub async fn has_conversation(&self, id: &str) -> Result<bool, sqlx::Error> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM conversations WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }

    /// Whether the chat's notifications are off at `now_ms` (false for a chat that isn't here).
    pub async fn is_muted(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let muted: Option<bool> = sqlx::query_scalar("SELECT mute_until > ?2 FROM conversations WHERE id = ?1")
            .bind(id)
            .bind(now_ms)
            .fetch_optional(&self.pool)
            .await?;
        Ok(muted.unwrap_or(false))
    }

    /// The chat was read: "marked unread" goes away (and the phone should learn it). True if it
    /// was set.
    pub async fn clear_marked_unread(&self, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE conversations SET marked_unread = 0, storage_dirty = 1 WHERE id = ?1 AND marked_unread = 1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    // ---- Storage Service sync state (see `crate::storage`) ----

    pub async fn storage_state(&self) -> Result<Option<StorageState>, sqlx::Error> {
        let row: Option<StorageSyncRow> = sqlx::query_as(
            "SELECT version, record_ikm, key_fingerprint, account_dirty, unmappable_pins FROM storage_sync WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(version, record_ikm, key_fingerprint, account_dirty, pins)| StorageState {
                version: u64::try_from(version).unwrap_or(0),
                record_ikm,
                key_fingerprint,
                account_dirty,
                unmappable_pins: unpack_blobs(&pins),
            },
        ))
    }

    /// `account_dirty` is taken only when there is no state yet: afterwards it moves only through
    /// `set_account_dirty` and `clear_account_dirty`, so a sync can't put back a stale value.
    pub async fn set_storage_state(&self, s: &StorageState) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO storage_sync(id, version, record_ikm, key_fingerprint, account_dirty, unmappable_pins)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version, record_ikm = excluded.record_ikm,
               key_fingerprint = excluded.key_fingerprint, unmappable_pins = excluded.unmappable_pins",
        )
        .bind(i64::try_from(s.version).unwrap_or(i64::MAX))
        .bind(&s.record_ikm)
        .bind(&s.key_fingerprint)
        .bind(s.account_dirty)
        .bind(pack_blobs(&s.unmappable_pins))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether the stored records were read for their contact details (names, notes,
    /// usernames). True when there's no state yet: the first pull reads every record.
    pub async fn storage_details_read(&self) -> Result<bool, sqlx::Error> {
        let read: Option<bool> = sqlx::query_scalar("SELECT details_read FROM storage_sync WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(read.unwrap_or(true))
    }

    pub async fn mark_storage_details_read(&self, read: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE storage_sync SET details_read = ?1 WHERE id = 1")
            .bind(read)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forgets the applied manifest and its items (a new storage key): the next pull starts
    /// from nothing. Dirty marks stay, the account's too.
    pub async fn reset_storage_state(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM storage_items").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM storage_sync WHERE account_dirty = 0")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE storage_sync SET version = 0, record_ikm = X'', key_fingerprint = X'', unmappable_pins = X''",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    pub async fn storage_items(&self) -> Result<Vec<StorageItemRow>, sqlx::Error> {
        let rows: Vec<(Vec<u8>, i64, Option<String>)> =
            sqlx::query_as("SELECT key, kind, conversation_id FROM storage_items ORDER BY rowid")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(key, kind, conversation_id)| StorageItemRow {
                key,
                kind: i32::try_from(kind).unwrap_or(0),
                conversation_id,
            })
            .collect())
    }

    /// The applied manifest's identifiers, replaced as a whole.
    pub async fn replace_storage_items(&self, rows: &[StorageItemRow]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM storage_items").execute(&mut *tx).await?;
        for r in rows {
            sqlx::query("INSERT INTO storage_items(key, kind, conversation_id) VALUES (?1, ?2, ?3)")
                .bind(&r.key)
                .bind(i64::from(r.kind))
                .bind(&r.conversation_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// The phone's values for a chat (created if we don't have it: it is listed once a message
    /// arrives). Remote wins; the chat's dirty mark goes. True if anything changed.
    pub async fn apply_remote_chat(&self, id: &str, f: &ChatFlags) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(id)
            .bind(kind_to_i64(kind))
            .execute(&mut *tx)
            .await?;
        let res = sqlx::query(
            "UPDATE conversations SET archived = ?2, marked_unread = ?3, mute_until = ?4, blocked = ?5,
               whitelisted = ?6, storage_dirty = 0,
               pinned_order = CASE WHEN ?2 THEN NULL ELSE pinned_order END
             WHERE id = ?1 AND (archived != ?2 OR marked_unread != ?3 OR mute_until != ?4 OR blocked != ?5
               OR whitelisted != ?6 OR storage_dirty != 0)",
        )
        .bind(id)
        .bind(f.archived)
        .bind(f.marked_unread)
        .bind(f.mute_until)
        .bind(f.blocked)
        .bind(f.whitelisted)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    /// The phone's pinned list replaces ours: `ids` in pin order (chats we don't have are
    /// created; a pinned chat is not archived). True if the list changed.
    pub async fn apply_remote_pins(&self, ids: &[String]) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let before: Vec<String> =
            sqlx::query_scalar("SELECT id FROM conversations WHERE pinned_order IS NOT NULL ORDER BY pinned_order")
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("UPDATE conversations SET pinned_order = NULL WHERE pinned_order IS NOT NULL")
            .execute(&mut *tx)
            .await?;
        for (i, id) in ids.iter().enumerate() {
            let kind = kind_of(id)?;
            sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
                .bind(id)
                .bind(kind_to_i64(kind))
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE conversations SET pinned_order = ?2, archived = 0 WHERE id = ?1")
                .bind(id)
                .bind(i64::try_from(i + 1).unwrap_or(i64::MAX))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(before != ids)
    }

    /// Note to Self's archived / marked unread from the account record (its other flags have no
    /// field there). Remote wins; true if either changed.
    pub async fn apply_remote_note_to_self(
        &self,
        id: &str,
        archived: bool,
        marked_unread: bool,
    ) -> Result<bool, sqlx::Error> {
        let kind = kind_of(id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(id)
            .bind(kind_to_i64(kind))
            .execute(&mut *tx)
            .await?;
        let res = sqlx::query(
            "UPDATE conversations SET archived = ?2, marked_unread = ?3, storage_dirty = 0,
               pinned_order = CASE WHEN ?2 THEN NULL ELSE pinned_order END
             WHERE id = ?1 AND (archived != ?2 OR marked_unread != ?3)",
        )
        .bind(id)
        .bind(archived)
        .bind(marked_unread)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    /// The pinned chats in pin order, listed or not.
    pub async fn pinned_chat_ids(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT id FROM conversations WHERE pinned_order IS NOT NULL ORDER BY pinned_order, id")
            .fetch_all(&self.pool)
            .await
    }

    /// A chat's flags, listed or not; None for a chat we don't have.
    pub async fn chat_flags(&self, id: &str) -> Result<Option<ChatFlags>, sqlx::Error> {
        let row: Option<(bool, bool, i64, bool, bool)> = sqlx::query_as(
            "SELECT archived, marked_unread, mute_until, blocked, whitelisted FROM conversations WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(archived, marked_unread, mute_until, blocked, whitelisted)| ChatFlags {
                archived,
                marked_unread,
                mute_until,
                blocked,
                whitelisted,
            },
        ))
    }

    /// Chats whose flags changed here and haven't reached the phone, by id.
    pub async fn dirty_chats(&self) -> Result<Vec<(String, ChatFlags)>, sqlx::Error> {
        let rows: Vec<(String, bool, bool, i64, bool, bool)> = sqlx::query_as(
            "SELECT id, archived, marked_unread, mute_until, blocked, whitelisted FROM conversations
             WHERE storage_dirty = 1 ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, archived, marked_unread, mute_until, blocked, whitelisted)| {
                (
                    id,
                    ChatFlags {
                        archived,
                        marked_unread,
                        mute_until,
                        blocked,
                        whitelisted,
                    },
                )
            })
            .collect())
    }

    /// Clears the marks of chats written with these flags; a chat whose flags changed since
    /// (while the write was in flight) stays dirty for the next push.
    pub async fn clear_dirty(&self, written: &[(String, ChatFlags)]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (id, f) in written {
            sqlx::query(
                "UPDATE conversations SET storage_dirty = 0
                 WHERE id = ?1 AND archived = ?2 AND marked_unread = ?3 AND mute_until = ?4
                   AND blocked = ?5 AND whitelisted = ?6",
            )
            .bind(id)
            .bind(f.archived)
            .bind(f.marked_unread)
            .bind(f.mute_until)
            .bind(f.blocked)
            .bind(f.whitelisted)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// A pin change is (or is no longer) waiting to be written.
    pub async fn set_account_dirty(&self, dirty: bool) -> Result<(), sqlx::Error> {
        let mut changes = self.account_changes.lock().await;
        if dirty {
            *changes += 1;
        }
        sqlx::query(
            "INSERT INTO storage_sync(id, account_dirty) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET account_dirty = excluded.account_dirty",
        )
        .bind(dirty)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many pin changes there have been: read before reading the pins to write.
    pub async fn account_changes(&self) -> u64 {
        *self.account_changes.lock().await
    }

    /// Clears the account's mark, unless a pin changed after `account_changes` returned `seen`.
    pub async fn clear_account_dirty(&self, seen: u64) -> Result<(), sqlx::Error> {
        let changes = self.account_changes.lock().await;
        if *changes == seen {
            sqlx::query("UPDATE storage_sync SET account_dirty = 0")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Whether local chat settings are written to the phone's account (on unless turned off).
    pub async fn storage_writes(&self) -> Result<bool, sqlx::Error> {
        let on: Option<bool> = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'storage_writes'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(on.unwrap_or(true))
    }

    pub async fn set_storage_writes(&self, on: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO settings VALUES ('storage_writes', ?1) ON CONFLICT DO UPDATE SET value = excluded.value",
        )
        .bind(on)
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
    /// The sticker our unsent message `key` is (see `insert_outgoing`), its emoji from the message.
    pub async fn outgoing_sticker(&self, key: &MessageKey) -> Result<Option<crate::send::SendSticker>, sqlx::Error> {
        // pack id, pack key, sticker id, the message's kind and its kind text (the emoji)
        type Row = (Vec<u8>, Vec<u8>, i64, i64, Option<String>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT s.pack_id, s.pack_key, s.sticker_id, m.kind, m.kind_text FROM outgoing_stickers s
             JOIN messages m ON m.author = s.author AND m.sent_at = s.sent_at
             WHERE s.author = ?1 AND s.sent_at = ?2",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(pack_id, pack_key, sticker_id, kind, emoji)| {
            Some(crate::send::SendSticker {
                pack_id,
                pack_key,
                sticker_id: u32::try_from(sticker_id).ok()?,
                emoji: if kind == 1 { emoji } else { None },
            })
        }))
    }

    pub async fn forget_outgoing_sticker(&self, key: &MessageKey) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM outgoing_stickers WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_outgoing(
        &self,
        m: &Message,
        files: &[OutgoingFile],
        preview: Option<&PreviewDraft>,
        sticker: Option<&crate::send::SendSticker>,
    ) -> Result<bool, sqlx::Error> {
        let dir = self
            .files
            .clone()
            .ok_or_else(|| sqlx::Error::Configuration("no attachments folder".into()))?;
        // Copies can be slow (large files, files still in iCloud): made before the transaction,
        // off the async threads, so the one database connection isn't held meanwhile.
        // the preview's image is copied last, as position -1 (a preview image's place)
        let preview_image = preview.and_then(|p| p.image.as_ref());
        let sources: Vec<String> = files.iter().chain(preview_image).map(|f| f.path.clone()).collect();
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
            let positioned = files.iter().zip(0i64..).chain(preview_image.map(|f| (f, -1)));
            for (i, (f, position)) in positioned.enumerate() {
                let id = add_outgoing_file(&mut tx, m, f, position, staged[i].1).await?;
                let file = id.to_string();
                std::fs::rename(dir.join(&names[i]), dir.join(&file))?;
                names[i] = file.clone();
                sqlx::query("UPDATE attachments SET file = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(&file)
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(p) = preview {
                sqlx::query("INSERT OR IGNORE INTO link_previews VALUES (?1, ?2, ?3, ?4, ?5)")
                    .bind(&m.author)
                    .bind(m.sent_at)
                    .bind(&p.url)
                    .bind(&p.title)
                    .bind(&p.description)
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(s) = sticker {
                sqlx::query("INSERT OR REPLACE INTO outgoing_stickers VALUES (?1, ?2, ?3, ?4, ?5)")
                    .bind(&m.author)
                    .bind(m.sent_at)
                    .bind(&s.pack_id)
                    .bind(&s.pack_key)
                    .bind(i64::from(s.sticker_id))
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

    /// Conversations that have messages: pinned ones first in their pin order, then the most
    /// recent first. Contacts known only by name (synced from the phone) aren't listed until a
    /// message arrives. Archived chats are listed too, flagged.
    pub async fn conversations(&self) -> Result<Vec<Conversation>, sqlx::Error> {
        let rows = sqlx::query(concat!(
            "SELECT c.*, ",
            is_request!(),
            " AS message_request, COALESCE(c.storage_name, c.title) AS display_title,
               COALESCE(c.chat_color, (SELECT value FROM app_values WHERE key = 'default_chat_color'))
                 AS shown_chat_color,
               COALESCE(c.wallpaper, (SELECT value FROM app_values WHERE key = 'default_wallpaper'))
                 AS shown_wallpaper, m.author AS last_author, m.is_outgoing AS last_outgoing, m.status AS last_status,
               m.deleted AS last_deleted, a.content_type AS file_type, a.file_name AS file_name,
               a.is_voice_note AS file_voice, m.sent_at AS last_sent_at, m.kind AS last_kind,
               m.kind_text AS last_kind_text, m.body_ranges AS last_ranges,
               (SELECT MAX(at) FROM chat_events e WHERE e.conversation_id = c.id AND e.kind = 0) AS failed_at
             FROM conversations c LEFT JOIN messages m ON m.id = (SELECT id FROM messages
               WHERE conversation_id = c.id ORDER BY sent_at DESC, author DESC LIMIT 1)
             LEFT JOIN attachments a ON a.message_author = m.author AND a.message_sent_at = m.sent_at
               AND a.position = 0
             WHERE c.last_message_at > 0
             ORDER BY c.pinned_order IS NULL, c.pinned_order, c.last_message_at DESC, c.id"
        ))
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
        value: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        let conv_kind = kind_of(conversation_id)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
            .bind(conversation_id)
            .bind(kind_to_i64(conv_kind))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO chat_events(conversation_id, at, kind, author, value) VALUES (?1, ?2, ?3, ?4, ?5)")
            .bind(conversation_id)
            .bind(at)
            .bind(kind.to_i64())
            .bind(author)
            .bind(value)
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
                // ours, view-once: presage's copy (the media's key, the caption) is all that's left
                if m.is_outgoing && m.kind == (MessageKind::ViewOnce { viewed: true }) {
                    self.queue_presage_forget(&m.conversation_id, m.sent_at).await?;
                }
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
                new_ranges,
                edited_at,
            } => {
                let mut tx = self.pool.begin().await?;
                let target = &resolve(&mut tx, target).await?;
                let res = sqlx::query(
                    "UPDATE messages SET body = ?3, body_ranges = ?5, edited = 1, edited_at = ?4
                     WHERE author = ?1 AND sent_at = ?2 AND deleted = 0 AND edited_at < ?4",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(new_body)
                .bind(edited_at)
                .bind(ranges_to_row(new_ranges))
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
                // a story deleted for everyone goes whole
                let story = sqlx::query("DELETE FROM stories WHERE author = ?1 AND sent_at = ?2")
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .execute(&mut *tx)
                    .await?;
                if story.rows_affected() == 1 {
                    let files = forget_story_files(&mut tx, &target.author, target.sent_at).await?;
                    tx.commit().await?;
                    self.remove_files(&files);
                    return Ok(vec![Applied::StoriesChanged]);
                }
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
                    "UPDATE messages SET body = NULL, quote_text = NULL, body_ranges = NULL, quote_ranges = NULL, deleted = 1
                     WHERE author = ?1 AND sent_at = ?2 AND deleted = 0",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .execute(&mut *tx)
                .await?;
                if res.rows_affected() == 1 {
                    // presage keeps the whole message (text, file keys): it goes too
                    forget_in_presage(&mut tx, target).await?;
                }
                let files = forget_deleted_text(&mut tx, target).await?;
                let m = finish_update(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                self.remove_files(&files);
                Ok(changed(m))
            }
            Op::PollCreated {
                message,
                options,
                allow_multiple,
            } => {
                let mut tx = self.pool.begin().await?;
                let options = serde_json::to_string(options).expect("strings serialize");
                // the same poll again (a resend under its timestamp) never changes what was voted on
                // and only a poll message here (not a text one sent under the same timestamp)
                let (poll_kind, _) = MessageKind::Poll { question: None }.to_row();
                let res = sqlx::query(
                    "INSERT OR IGNORE INTO polls(author, sent_at, options, allow_multiple)
                     SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM messages
                       WHERE author = ?1 AND sent_at = ?2 AND kind = ?5 AND deleted = 0)",
                )
                .bind(&message.author)
                .bind(message.sent_at)
                .bind(options)
                .bind(allow_multiple)
                .bind(poll_kind)
                .execute(&mut *tx)
                .await?;
                let m = finish_poll(&mut tx, message, res.rows_affected()).await?;
                tx.commit().await?;
                Ok(changed(m))
            }
            Op::PollVote {
                conversation_id,
                target,
                voter,
                vote_count,
                options,
            } => {
                let mut tx = self.pool.begin().await?;
                if !is_open_poll_in(&mut tx, conversation_id, target).await? {
                    return Ok(vec![]);
                }
                let options = serde_json::to_string(options).expect("indexes serialize");
                let res = sqlx::query(
                    "INSERT INTO poll_votes(target_author, target_sent_at, voter, vote_count, options)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(target_author, target_sent_at, voter) DO UPDATE
                       SET vote_count = ?4, options = ?5 WHERE ?4 > poll_votes.vote_count",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(voter)
                .bind(i64::from(*vote_count))
                .bind(options)
                .execute(&mut *tx)
                .await?;
                let m = finish_poll(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                Ok(changed(m))
            }
            Op::PollEnded {
                conversation_id,
                target,
            } => {
                let mut tx = self.pool.begin().await?;
                if !is_open_poll_in(&mut tx, conversation_id, target).await? {
                    return Ok(vec![]);
                }
                let res = sqlx::query("UPDATE polls SET ended = 1 WHERE author = ?1 AND sent_at = ?2 AND ended = 0")
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .execute(&mut *tx)
                    .await?;
                let m = finish_poll(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                Ok(changed(m))
            }
            Op::InsertStory(s) => {
                let mut tx = self.pool.begin().await?;
                // deleted for everyone before it came
                let deleted = sqlx::query("DELETE FROM pending_deletes WHERE author = ?1 AND sent_at = ?2")
                    .bind(&s.author)
                    .bind(s.sent_at)
                    .execute(&mut *tx)
                    .await?;
                if deleted.rows_affected() == 1 {
                    tx.commit().await?;
                    return Ok(vec![]);
                }
                let text = s.text.as_ref();
                let res = sqlx::query(
                    "INSERT OR IGNORE INTO stories(author, sent_at, conversation_id, received_at, text, text_style,
                       text_foreground, text_background, background, angle, link, outgoing)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .bind(&s.author)
                .bind(s.sent_at)
                .bind(&s.conversation_id)
                .bind(s.received_at)
                .bind(text.map(|t| &t.text))
                .bind(text.map_or(0, |t| i64::from(t.style)))
                .bind(text.and_then(|t| t.foreground).map(i64::from))
                .bind(text.and_then(|t| t.text_background).map(i64::from))
                .bind(
                    serde_json::to_string(&text.map(|t| t.background.clone()).unwrap_or_default())
                        .expect("colours serialize"),
                )
                .bind(text.map_or(0, |t| i64::from(t.angle)))
                .bind(text.and_then(|t| t.link.as_ref()))
                .bind(s.outgoing)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(if res.rows_affected() == 1 {
                    vec![Applied::StoriesChanged]
                } else {
                    vec![]
                })
            }
            Op::ViewOnceOpened { target } => {
                let mut tx = self.pool.begin().await?;
                let target = &resolve(&mut tx, target).await?;
                let (unviewed, _) = MessageKind::ViewOnce { viewed: false }.to_row();
                let (_, viewed) = MessageKind::ViewOnce { viewed: true }.to_row();
                let res = sqlx::query(
                    "UPDATE messages SET kind_text = ?4
                     WHERE author = ?1 AND sent_at = ?2 AND kind = ?3 AND kind_text IS NULL",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(unviewed)
                .bind(viewed)
                .execute(&mut *tx)
                .await?;
                let files: Vec<Option<String>> = if res.rows_affected() == 1 {
                    forget_in_presage(&mut tx, target).await?;
                    sqlx::query_scalar(
                        "DELETE FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 RETURNING file",
                    )
                    .bind(&target.author)
                    .bind(target.sent_at)
                    .fetch_all(&mut *tx)
                    .await?
                } else {
                    vec![]
                };
                let m = finish_update(&mut tx, target, res.rows_affected()).await?;
                tx.commit().await?;
                self.remove_files(&files.into_iter().flatten().collect::<Vec<_>>());
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
                link_previews,
            } => {
                let mut tx = self.pool.begin().await?;
                for (key, value) in [
                    ("read_receipts", read_receipts),
                    ("typing_indicators", typing_indicators),
                    ("link_previews", link_previews),
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
            Op::BlockedList { acis, groups } => {
                let mut listed: Vec<String> = acis.iter().map(|a| format!("c:{a}")).collect();
                for g in groups {
                    listed.extend(self.group_by_identifier(g).await?);
                }
                let mut changed = false;
                // not unblocked: a block made here the phone hasn't seen yet (still to write),
                // or a PNI chat (a list can't name it)
                let unlisted: Vec<String> = sqlx::query_scalar(
                    "SELECT id FROM conversations WHERE blocked = 1 AND storage_dirty = 0
                       AND id NOT LIKE 'c:PNI:%' ORDER BY id",
                )
                .fetch_all(&self.pool)
                .await?;
                for id in unlisted {
                    if !listed.contains(&id) {
                        changed |= self.apply_phone_block(&id, false).await?;
                    }
                }
                for id in &listed {
                    changed |= self.apply_phone_block(id, true).await?;
                }
                Ok(if changed {
                    vec![Applied::ConversationsChanged]
                } else {
                    vec![]
                })
            }
            Op::RequestResponse { thread, response } => {
                let Some(id) = self.request_chat(thread).await? else {
                    return Ok(vec![]);
                };
                let mut changed = match response {
                    RequestResponse::Accept => self.apply_phone_block(&id, false).await?,
                    RequestResponse::Block | RequestResponse::BlockAndDelete => {
                        self.apply_phone_block(&id, true).await?
                    }
                    RequestResponse::Delete => false,
                };
                if matches!(response, RequestResponse::Delete | RequestResponse::BlockAndDelete) {
                    let files = self.delete_chat_in(&id, false).await?;
                    self.remove_files(&files);
                    changed = true;
                }
                Ok(if changed {
                    vec![Applied::ConversationsChanged]
                } else {
                    vec![]
                })
            }
            Op::SetStatus { targets, status } => self.set_status(targets, *status, None).await,
            Op::Read { targets, at } => self.set_status(targets, MessageStatus::Read, Some(*at)).await,
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
                    // a redelivered change is shown once
                    "INSERT INTO chat_events(conversation_id, at, kind, author, value)
                     SELECT ?1, ?2, ?3, ?4, ?5 WHERE NOT EXISTS (SELECT 1 FROM chat_events
                       WHERE conversation_id = ?1 AND at = ?2 AND kind = ?3 AND author = ?4)",
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

    /// Raises the status of `targets`; reads start disappearing timers (from `read_at` if known,
    /// never later than now).
    async fn set_status(
        &self,
        targets: &[MessageKey],
        status: MessageStatus,
        read_at: Option<i64>,
    ) -> Result<Vec<Applied>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let mut out = vec![];
        for target in targets {
            let target = &resolve(&mut tx, target).await?;
            if conversation_of(&mut tx, target).await?.is_none() {
                // e.g. read on the phone before the message reached us
                sqlx::query(
                    "INSERT INTO pending_status(author, sent_at, status, read_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT DO UPDATE SET status = MAX(status, excluded.status),
                       read_at = COALESCE(read_at, excluded.read_at)",
                )
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(status.to_i64())
                .bind(read_at)
                .execute(&mut *tx)
                .await?;
                continue;
            }
            let res = sqlx::query("UPDATE messages SET status = ?3 WHERE author = ?1 AND sent_at = ?2 AND status < ?3")
                .bind(&target.author)
                .bind(target.sent_at)
                .bind(status.to_i64())
                .execute(&mut *tx)
                .await?;
            if status == MessageStatus::Read {
                start_timer(&mut tx, target, read_at).await?;
            }
            if let Some((conversation_id, key)) = finish_update(&mut tx, target, res.rows_affected()).await? {
                out.push(Applied::MessageChanged { conversation_id, key });
            }
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Sets a chat's timer as it really is (presage keeps it), without a chat event.
    pub async fn set_chat_timer(&self, conversation_id: &str, seconds: u32) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("UPDATE conversations SET expire_timer = ?2 WHERE id = ?1 AND expire_timer != ?2")
            .bind(conversation_id)
            .bind(i64::from(seconds))
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Sets the timer one of our sent messages went out with (presage chose it); it started when
    /// the message was sent.
    pub async fn set_message_timer(&self, key: &MessageKey, seconds: u32) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE messages SET expires_in = ?3,
               expires_at = CASE WHEN ?3 > 0 THEN sent_at + ?3 * 1000 ELSE NULL END
             WHERE author = ?1 AND sent_at = ?2",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .bind(i64::from(seconds))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// presage copies of expired messages still to delete: (conversation, timestamp).
    pub async fn presage_forget_queue(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as("SELECT conversation_id, sent_at FROM presage_forget")
            .fetch_all(&self.pool)
            .await
    }

    /// One queued presage copy is gone.
    pub async fn presage_forgotten(&self, conversation_id: &str, sent_at: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM presage_forget WHERE conversation_id = ?1 AND sent_at = ?2")
            .bind(conversation_id)
            .bind(sent_at)
            .execute(&self.pool)
            .await?;
        Ok(())
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
    /// previews and files. Returns them. presage's copies of them and their edits are queued
    /// (`presage_forget`) in the same transaction, so none is forgotten if deleting there fails.
    pub async fn delete_expired(&self, now_ms: i64) -> Result<Vec<Expired>, sqlx::Error> {
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
            let revisions: Vec<i64> =
                sqlx::query_scalar("SELECT sent_at FROM message_revisions WHERE author = ?1 AND original_sent_at = ?2")
                    .bind(&key.author)
                    .bind(key.sent_at)
                    .fetch_all(&mut *tx)
                    .await?;
            for at in std::iter::once(key.sent_at).chain(revisions) {
                sqlx::query("INSERT OR IGNORE INTO presage_forget(conversation_id, sent_at) VALUES (?1, ?2)")
                    .bind(&conversation_id)
                    .bind(at)
                    .execute(&mut *tx)
                    .await?;
            }
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
            out.push(Expired { conversation_id, key });
        }
        let mut conversations: Vec<&String> = out.iter().map(|e| &e.conversation_id).collect();
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
        let t: Option<String> = sqlx::query_scalar(
            "SELECT COALESCE(storage_name, NULLIF(title, '')) FROM conversations WHERE id = ?1
               AND COALESCE(storage_name, NULLIF(title, '')) IS NOT NULL",
        )
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
            link_previews: get("link_previews"),
        })
    }

    /// Remembers that read receipts for `targets` still have to go out.
    pub async fn queue_receipts(&self, targets: &[MessageKey]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            sqlx::query("INSERT OR IGNORE INTO unsent_receipts(author, sent_at) VALUES (?1, ?2)")
                .bind(&t.author)
                .bind(t.sent_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Reads not yet told to our other devices (whatever the chat), by sender then time.
    pub async fn unsynced_reads(&self) -> Result<Vec<MessageKey>, sqlx::Error> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT author, sent_at FROM unsent_receipts WHERE synced = 0 ORDER BY author, sent_at")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(author, sent_at)| MessageKey { author, sent_at })
            .collect())
    }

    /// Our other devices know these were read.
    pub async fn reads_synced(&self, targets: &[MessageKey]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            sqlx::query("UPDATE unsent_receipts SET synced = 1 WHERE author = ?1 AND sent_at = ?2")
                .bind(&t.author)
                .bind(t.sent_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Read receipts still to send to their senders, by sender then time: only once our other
    /// devices know (so removing a row never loses a read sync), and not those of message
    /// requests and blocked chats.
    pub async fn unsent_receipts(&self) -> Result<Vec<MessageKey>, sqlx::Error> {
        // a request's or blocked chat's wait until it is accepted (or unblocked)
        let rows: Vec<(String, i64)> = sqlx::query_as(concat!(
            "SELECT u.author, u.sent_at FROM unsent_receipts u
             WHERE u.synced = 1 AND NOT EXISTS (SELECT 1 FROM messages m JOIN conversations c ON c.id = m.conversation_id
               WHERE m.author = u.author AND m.sent_at = u.sent_at AND (c.blocked OR ",
            is_request!(),
            ")) ORDER BY u.author, u.sent_at"
        ))
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
            "SELECT id FROM attachments a WHERE state = ?1 AND pointer IS NOT NULL AND size <= ?2
               AND (content_type LIKE 'image/%' OR is_voice_note = 1)
               -- a story's file is fetched when the story is opened, not before
               AND NOT EXISTS (SELECT 1 FROM stories s WHERE s.author = a.message_author AND s.sent_at = a.message_sent_at)
             ORDER BY id",
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

    /// The in-flight history import, if one was started and not finished or abandoned.
    pub async fn history_state(&self) -> Result<Option<HistoryImportState>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT stage, key, aci, cdn, cdn_key, bytes_done, total_bytes FROM history_import WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let stage: String = r.try_get("stage")?;
            let cdn: Option<i64> = r.try_get("cdn")?;
            let bytes_done: i64 = r.try_get("bytes_done")?;
            let total: Option<i64> = r.try_get("total_bytes")?;
            Ok(HistoryImportState {
                stage: HistoryDbStage::parse(&stage).ok_or_else(|| decode_err(format!("history stage {stage:?}")))?,
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

    /// The import finished or was abandoned: nothing resumes on the next launch.
    pub async fn clear_history_state(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM history_import").execute(&self.pool).await?;
        Ok(())
    }

    /// Writes one batch of imported history in one transaction, merging into what is already
    /// here: existing messages keep their content (only the read status can rise), existing
    /// reactions, files and titles win, and a repeated import changes nothing. Emits no events;
    /// the caller announces once at the end of the whole import.
    pub(crate) async fn import_history(&self, batch: &crate::history::import::HistoryBatch) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let mut touched = std::collections::BTreeSet::new();
        for c in &batch.conversations {
            let kind = kind_of(&c.id)?;
            // the phone's chat flags win: at link time they are the truth
            sqlx::query(
                "INSERT INTO conversations(id, kind, title, expire_timer, archived, pinned_order, mute_until,
                   marked_unread, chat_color, wallpaper)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET
                   title = CASE WHEN conversations.title = '' THEN excluded.title ELSE conversations.title END,
                   archived = excluded.archived, pinned_order = excluded.pinned_order,
                   mute_until = excluded.mute_until, marked_unread = excluded.marked_unread,
                   chat_color = excluded.chat_color, wallpaper = excluded.wallpaper",
            )
            .bind(&c.id)
            .bind(kind_to_i64(kind))
            .bind(&c.title)
            .bind(i64::from(c.expire_timer))
            .bind(c.archived)
            .bind(c.pinned_order.map(i64::from))
            .bind(c.mute_until)
            .bind(c.marked_unread)
            .bind(c.chat_color.as_ref().map(ChatColor::to_row))
            .bind(c.wallpaper.as_ref().map(ChatColor::to_row))
            .execute(&mut *tx)
            .await?;
        }
        // the phone's defaults, set or removed: only from the batch that read them
        if batch.defaults_read {
            for (key, value) in [
                ("default_chat_color", &batch.default_chat_color),
                ("default_wallpaper", &batch.default_wallpaper),
            ] {
                match value {
                    Some(v) => {
                        sqlx::query("INSERT OR REPLACE INTO app_values VALUES (?1, ?2)")
                            .bind(key)
                            .bind(v.to_row())
                            .execute(&mut *tx)
                            .await?
                    }
                    None => {
                        sqlx::query("DELETE FROM app_values WHERE key = ?1")
                            .bind(key)
                            .execute(&mut *tx)
                            .await?
                    }
                };
            }
        }
        for m in &batch.messages {
            let msg = &m.message;
            let kind = kind_of(&msg.conversation_id)?;
            sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
                .bind(&msg.conversation_id)
                .bind(kind_to_i64(kind))
                .execute(&mut *tx)
                .await?;
            let quote = msg.quote.as_ref();
            let (body, quote_text, body_ranges, quote_ranges) = if msg.deleted {
                (None, None, None, None)
            } else {
                (
                    msg.body.as_ref(),
                    quote.and_then(|q| q.text.as_ref()),
                    ranges_to_row(&msg.ranges),
                    quote.and_then(|q| ranges_to_row(&q.ranges)),
                )
            };
            let (msg_kind, kind_text) = msg.kind.to_row();
            sqlx::query(
                "INSERT INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
                   status, edited, deleted, quote_author, quote_sent_at, quote_text, kind, kind_text,
                   expires_in, expires_at, edited_at, body_ranges, quote_ranges)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(author, sent_at) DO UPDATE SET
                   status = MAX(messages.status, excluded.status),
                   expires_at = COALESCE(MIN(messages.expires_at, excluded.expires_at),
                                         messages.expires_at, excluded.expires_at)",
            )
            .bind(&msg.conversation_id)
            .bind(&msg.author)
            .bind(msg.sent_at)
            .bind(msg.received_at)
            .bind(body)
            .bind(msg.is_outgoing)
            .bind(msg.status.to_i64())
            .bind(msg.edited)
            .bind(msg.deleted)
            .bind(quote.map(|q| &q.author))
            .bind(quote.map(|q| q.sent_at))
            .bind(quote_text)
            .bind(msg_kind)
            .bind(kind_text)
            .bind(i64::from(msg.expires_in))
            .bind(msg.expires_at)
            .bind(m.edited_at)
            .bind(body_ranges)
            .bind(quote_ranges)
            .execute(&mut *tx)
            .await?;
            for revision_sent_at in &m.revisions {
                // live edits, deletes and reactions may target the message by any revision's
                // timestamp; map them all to the original
                sqlx::query("INSERT OR IGNORE INTO message_revisions VALUES (?1, ?2, ?3)")
                    .bind(&msg.author)
                    .bind(revision_sent_at)
                    .bind(msg.sent_at)
                    .execute(&mut *tx)
                    .await?;
            }
            if let Some(p) = &m.poll {
                sqlx::query(
                    "INSERT INTO polls(author, sent_at, options, allow_multiple, ended) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(author, sent_at) DO UPDATE SET options = ?3, allow_multiple = ?4,
                       ended = MAX(ended, ?5)",
                )
                .bind(&msg.author)
                .bind(msg.sent_at)
                .bind(serde_json::to_string(&p.options).expect("strings serialize"))
                .bind(p.allow_multiple)
                .bind(p.ended)
                .execute(&mut *tx)
                .await?;
                for (voter, count, picks) in &p.votes {
                    // a newer vote that came live stays
                    sqlx::query(
                        "INSERT INTO poll_votes(target_author, target_sent_at, voter, vote_count, options)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(target_author, target_sent_at, voter) DO UPDATE
                           SET vote_count = ?4, options = ?5 WHERE ?4 > poll_votes.vote_count",
                    )
                    .bind(&msg.author)
                    .bind(msg.sent_at)
                    .bind(voter)
                    .bind(i64::from(*count))
                    .bind(serde_json::to_string(picks).expect("indexes serialize"))
                    .execute(&mut *tx)
                    .await?;
                }
            }
            for (reactor, emoji) in &m.reactions {
                sqlx::query(
                    // never onto a message deleted here: the archive predates the deletion
                    "INSERT OR IGNORE INTO reactions(target_author, target_sent_at, reactor, emoji)
                     SELECT ?1, ?2, ?3, ?4
                     WHERE (SELECT deleted FROM messages WHERE author = ?1 AND sent_at = ?2) = 0",
                )
                .bind(&msg.author)
                .bind(msg.sent_at)
                .bind(reactor)
                .bind(emoji)
                .execute(&mut *tx)
                .await?;
            }
            let preview_image = m.preview.as_ref().and_then(|p| p.image.as_ref());
            for a in m.attachments.iter().chain(preview_image) {
                let state = if a.pointer.is_some() {
                    AttachmentState::Pending
                } else {
                    // no longer on the transit CDN: shown as unavailable
                    AttachmentState::Failed
                };
                sqlx::query(
                    "INSERT OR IGNORE INTO attachments(message_author, message_sent_at, position,
                       content_type, file_name, size, width, height, is_voice_note, pointer, state)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11
                     WHERE (SELECT deleted FROM messages WHERE author = ?1 AND sent_at = ?2) = 0",
                )
                .bind(&msg.author)
                .bind(msg.sent_at)
                .bind(a.position)
                .bind(&a.content_type)
                .bind(&a.file_name)
                .bind(a.size as i64)
                .bind(a.width.map(i64::from))
                .bind(a.height.map(i64::from))
                .bind(a.is_voice_note)
                .bind(&a.pointer)
                .bind(state.to_i64())
                .execute(&mut *tx)
                .await?;
            }
            if let Some(p) = &m.preview {
                sqlx::query(
                    "INSERT OR IGNORE INTO link_previews(message_author, message_sent_at, url, title, description)
                     SELECT ?1, ?2, ?3, ?4, ?5
                     WHERE (SELECT deleted FROM messages WHERE author = ?1 AND sent_at = ?2) = 0",
                )
                .bind(&msg.author)
                .bind(msg.sent_at)
                .bind(&p.url)
                .bind(&p.title)
                .bind(&p.description)
                .execute(&mut *tx)
                .await?;
            }
            touched.insert(msg.conversation_id.clone());
        }
        for e in &batch.events {
            let kind = kind_of(&e.conversation_id)?;
            sqlx::query("INSERT OR IGNORE INTO conversations(id, kind) VALUES (?1, ?2)")
                .bind(&e.conversation_id)
                .bind(kind_to_i64(kind))
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                // a change both imported and received live is shown once
                "INSERT INTO chat_events(conversation_id, at, kind, author, value)
                 SELECT ?1, ?2, ?3, ?4, ?5 WHERE NOT EXISTS (SELECT 1 FROM chat_events
                   WHERE conversation_id = ?1 AND at = ?2 AND kind = ?3 AND author = ?4)",
            )
            .bind(&e.conversation_id)
            .bind(e.at)
            .bind(ChatEventKind::TimerChanged.to_i64())
            .bind(&e.author)
            .bind(e.value)
            .execute(&mut *tx)
            .await?;
            touched.insert(e.conversation_id.clone());
        }
        for id in &touched {
            refresh_summary(&mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(())
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

/// Whether `key` is a poll here, in `conversation_id`, not deleted and not ended: a vote or end
/// names its poll by author and time only, and must not reach one in a chat its sender isn't
/// in, nor change one that's over.
async fn is_open_poll_in(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    key: &MessageKey,
) -> Result<bool, sqlx::Error> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM messages m JOIN polls p ON p.author = m.author AND p.sent_at = m.sent_at
         WHERE m.author = ?1 AND m.sent_at = ?2 AND m.conversation_id = ?3 AND m.deleted = 0
           AND p.ended = 0",
    )
    .bind(&key.author)
    .bind(key.sent_at)
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(found.is_some())
}

/// The poll message's chat, if a change to it took and the message is shown (not deleted).
async fn finish_poll(
    conn: &mut SqliteConnection,
    key: &MessageKey,
    rows: u64,
) -> Result<Option<(String, MessageKey)>, sqlx::Error> {
    if rows == 0 || is_deleted(conn, key).await? {
        return Ok(None);
    }
    conversation_of(conn, key).await
}

/// Deletes a story's file rows; returns the files to remove once committed.
async fn forget_story_files(
    conn: &mut SqliteConnection,
    author: &str,
    sent_at: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let files: Vec<Option<String>> =
        sqlx::query_scalar("DELETE FROM attachments WHERE message_author = ?1 AND message_sent_at = ?2 RETURNING file")
            .bind(author)
            .bind(sent_at)
            .fetch_all(conn)
            .await?;
    Ok(files.into_iter().flatten().collect())
}

/// Queues presage's copies of the message `key` for the sweep to delete (`presage_forget`): the
/// original and each edit, which presage keeps under its own timestamp.
async fn forget_in_presage(conn: &mut SqliteConnection, key: &MessageKey) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO presage_forget(conversation_id, sent_at)
         SELECT conversation_id, sent_at FROM messages WHERE author = ?1 AND sent_at = ?2
         UNION SELECT m.conversation_id, r.sent_at FROM message_revisions r
           JOIN messages m ON m.author = r.author AND m.sent_at = r.original_sent_at
           WHERE r.author = ?1 AND r.original_sent_at = ?2",
    )
    .bind(&key.author)
    .bind(key.sent_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Drops what other rows still hold of a deleted message: its reactions, quotes of its text,
/// link preview, poll and votes. Returns the attachment files to remove once the transaction is
/// committed.
async fn forget_deleted_text(conn: &mut SqliteConnection, key: &MessageKey) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query("DELETE FROM reactions WHERE target_author = ?1 AND target_sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE messages SET quote_text = NULL, quote_ranges = NULL WHERE quote_author = ?1 AND quote_sent_at = ?2",
    )
    .bind(&key.author)
    .bind(key.sent_at)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM polls WHERE author = ?1 AND sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM outgoing_stickers WHERE author = ?1 AND sent_at = ?2")
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM poll_votes WHERE target_author = ?1 AND target_sent_at = ?2")
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
    let (body, quote_text, body_ranges, quote_ranges) = if m.deleted {
        (None, None, None, None)
    } else {
        (
            m.body.as_ref(),
            quote.and_then(|q| q.text.as_ref()),
            ranges_to_row(&m.ranges),
            quote.and_then(|q| ranges_to_row(&q.ranges)),
        )
    };
    let (kind, kind_text) = m.kind.to_row();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO messages(conversation_id, author, sent_at, received_at, body, is_outgoing,
           status, edited, deleted, quote_author, quote_sent_at, quote_text, kind, kind_text, expires_in,
           expires_at, body_ranges, quote_ranges)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
    .bind(body_ranges)
    .bind(quote_ranges)
    .execute(&mut *conn)
    .await?;
    let inserted = res.rows_affected() == 1;
    if inserted {
        if m.is_outgoing {
            // writing in an archived chat brings it back (Desktop's rule; an incoming message doesn't)
            sqlx::query("UPDATE conversations SET archived = 0 WHERE id = ?1 AND archived = 1")
                .bind(&m.conversation_id)
                .execute(&mut *conn)
                .await?;
        }
        // (the chat's timer comes from presage, which orders changes by version: see the actor)
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

/// Starts a read message's disappearing timer (once), from when it was read (`read_at`, never
/// later than now): a read on the phone that reaches us late must not keep it longer here.
async fn start_timer(conn: &mut SqliteConnection, key: &MessageKey, read_at: Option<i64>) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "UPDATE messages SET expires_at = ?3 + expires_in * 1000
         WHERE author = ?1 AND sent_at = ?2 AND expires_in > 0 AND expires_at IS NULL",
    )
    .bind(&key.author)
    .bind(key.sent_at)
    .bind(read_at.map_or(now, |at| at.min(now)))
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
        forget_in_presage(&mut *conn, key).await?;
        sqlx::query(
            "UPDATE messages SET body = NULL, quote_text = NULL, body_ranges = NULL, quote_ranges = NULL, deleted = 1 WHERE author = ?1 AND sent_at = ?2",
        )
        .bind(&key.author)
        .bind(key.sent_at)
        .execute(&mut *conn)
        .await?;
        // nothing is attached yet: attachments are added after the message
        forget_deleted_text(conn, key).await?;
    }
    let pending: Option<(i64, Option<i64>)> =
        sqlx::query_as("DELETE FROM pending_status WHERE author = ?1 AND sent_at = ?2 RETURNING status, read_at")
            .bind(&key.author)
            .bind(key.sent_at)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((status, read_at)) = pending {
        sqlx::query("UPDATE messages SET status = MAX(status, ?3) WHERE author = ?1 AND sent_at = ?2")
            .bind(&key.author)
            .bind(key.sent_at)
            .bind(status)
            .execute(&mut *conn)
            .await?;
        if status >= MessageStatus::Read.to_i64() {
            start_timer(conn, key, read_at).await?;
        }
    }
    Ok(())
}

/// `storage_sync`'s columns as sqlx reads them: version, record_ikm, key_fingerprint, account_dirty, unmappable_pins.
type StorageSyncRow = (i64, Vec<u8>, Vec<u8>, bool, Vec<u8>);

/// `u32 LE length || bytes` per entry (the `unmappable_pins` column).
fn pack_blobs(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|b| b.len() + 4).sum());
    for b in items {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

fn unpack_blobs(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    while b.len() >= 4 {
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let Some(item) = b.get(4..4 + len) else { break };
        out.push(item.to_vec());
        b = &b[4 + len..];
    }
    out
}

/// The conversation a stored message belongs to, if it is stored.
async fn conversation_of(
    conn: &mut SqliteConnection,
    key: &MessageKey,
) -> Result<Option<(String, MessageKey)>, sqlx::Error> {
    // a story's file is listed and downloaded like a message's
    let conv: Option<String> = sqlx::query_scalar(
        "SELECT conversation_id FROM messages WHERE author = ?1 AND sent_at = ?2
         UNION ALL SELECT conversation_id FROM stories WHERE author = ?1 AND sent_at = ?2 LIMIT 1",
    )
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
        title: r.try_get("display_title")?,
        last_message_at: r.try_get("last_message_at")?,
        last_message_preview: r.try_get("last_message_preview")?,
        last_message_ranges: if undecryptable {
            vec![]
        } else {
            ranges_from_row(r.try_get("last_ranges")?)
        },
        unread_count: u32::try_from(unread).map_err(|_| decode_err(format!("unread_count {unread}")))?,
        archived: r.try_get("archived")?,
        pinned_order: r
            .try_get::<Option<i64>, _>("pinned_order")?
            .map(|o| u32::try_from(o).map_err(|_| decode_err(format!("pinned_order {o}"))))
            .transpose()?,
        mute_until: r.try_get("mute_until")?,
        marked_unread: r.try_get("marked_unread")?,
        blocked: r.try_get("blocked")?,
        whitelisted: r.try_get("whitelisted")?,
        message_request: r.try_get("message_request")?,
        avatar: r.try_get("avatar")?,
        username: r.try_get("username")?,
        note: r.try_get("note")?,
        chat_color: r
            .try_get::<Option<String>, _>("shown_chat_color")?
            .and_then(|c| ChatColor::from_row(&c)),
        wallpaper: r
            .try_get::<Option<String>, _>("shown_wallpaper")?
            .and_then(|c| ChatColor::from_row(&c)),
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
        ranges: ranges_from_row(r.try_get("body_ranges")?),
        is_outgoing: r.try_get("is_outgoing")?,
        status: MessageStatus::from_i64(status).ok_or_else(|| decode_err(format!("message status {status}")))?,
        edited: r.try_get("edited")?,
        deleted: r.try_get("deleted")?,
        quote: match (quote_author, quote_sent_at) {
            (Some(author), Some(sent_at)) => Some(Quote {
                author,
                sent_at,
                text: r.try_get("quote_text")?,
                ranges: ranges_from_row(r.try_get("quote_ranges")?),
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
            ranges: vec![],
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

    #[tokio::test]
    async fn a_missed_call_shows_in_the_chat_with_duration_support() {
        let db = db().await;
        db.add_chat_event("c:alice", 50, ChatEventKind::MissedCall, "alice", None)
            .await
            .unwrap();
        db.add_chat_event("c:alice", 90, ChatEventKind::IncomingCall, "alice", Some(125))
            .await
            .unwrap();
        let events = db.chat_events("c:alice", 0).await.unwrap();
        assert_eq!(
            events.iter().map(|e| (e.kind, e.value)).collect::<Vec<_>>(),
            vec![
                (ChatEventKind::MissedCall, None),
                (ChatEventKind::IncomingCall, Some(125))
            ]
        );
    }

    #[tokio::test]
    async fn history_state_round_trips_and_clears() {
        let db = db().await;
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

    #[test]
    fn the_archive_key_never_prints() {
        let s = HistoryImportState {
            stage: HistoryDbStage::Waiting,
            key: vec![0xAB; 32],
            aci: "a".into(),
            cdn: None,
            cdn_key: None,
            bytes_done: 0,
            total_bytes: None,
        };
        let printed = format!("{s:?}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(!printed.contains("171"), "no key bytes: {printed}");
    }

    mod import_history {
        use super::*;
        use crate::history::import::{HistoryBatch, ImportAttachment, ImportConversation, ImportEvent, ImportMessage};

        fn imported(conv: &str, author: &str, sent_at: i64, body: &str, status: MessageStatus) -> ImportMessage {
            ImportMessage {
                message: Message {
                    status,
                    ..in_conv(conv, msg(author, sent_at, body))
                },
                reactions: vec![],
                attachments: vec![],
                preview: None,
                revisions: vec![],
                edited_at: 0,
                poll: None,
            }
        }

        fn batch_with(messages: Vec<ImportMessage>) -> HistoryBatch {
            HistoryBatch {
                conversations: vec![],
                messages,
                events: vec![],
                default_chat_color: None,
                default_wallpaper: None,
                defaults_read: false,
            }
        }

        fn key(author: &str, sent_at: i64) -> MessageKey {
            MessageKey {
                author: author.into(),
                sent_at,
            }
        }

        #[tokio::test]
        async fn an_existing_message_is_kept_but_status_rises() {
            let db = db().await;
            db.insert_message(&in_conv("c:alice", msg("alice", 5, "live")))
                .await
                .unwrap();
            db.import_history(&batch_with(vec![imported(
                "c:alice",
                "alice",
                5,
                "archived",
                MessageStatus::Read,
            )]))
            .await
            .unwrap();
            let m = db.message(&key("alice", 5)).await.unwrap().unwrap();
            assert_eq!((m.body.as_deref(), m.status), (Some("live"), MessageStatus::Read));
        }

        #[tokio::test]
        async fn imported_styles_and_mentions_are_kept() {
            let db = db().await;
            let mut m = imported("c:alice", "alice", 5, "hi", MessageStatus::Read);
            let range = |style| BodyRange {
                start: 0,
                length: 2,
                style,
            };
            m.message.ranges = vec![range(TextStyle::Italic)];
            m.message.quote = Some(Quote {
                author: "me".into(),
                sent_at: 1,
                text: Some("q".into()),
                ranges: vec![range(TextStyle::Spoiler)],
            });
            let want = m.message.clone();
            db.import_history(&batch_with(vec![m])).await.unwrap();
            assert_eq!(db.message(&key("alice", 5)).await.unwrap().unwrap(), want);
        }

        #[tokio::test]
        async fn an_imported_poll_keeps_its_votes_and_newer_live_ones_win() {
            let db = db().await;
            let mut m = imported("c:alice", "alice", 5, "", MessageStatus::Read);
            m.message.body = None;
            m.message.kind = MessageKind::Poll {
                question: Some("Lunch?".into()),
            };
            m.poll = Some(crate::history::import::ImportPoll {
                options: vec!["A".into(), "B".into()],
                allow_multiple: false,
                ended: false,
                votes: vec![("bob".into(), 2, vec![0])],
            });
            db.import_history(&batch_with(vec![m])).await.unwrap();
            // a newer live vote after it counts, and importing again doesn't undo it
            db.apply(&Op::PollVote {
                conversation_id: "c:alice".into(),
                target: key("alice", 5),
                voter: "bob".into(),
                vote_count: 4,
                options: vec![1],
            })
            .await
            .unwrap();
            let mut again = imported("c:alice", "alice", 5, "", MessageStatus::Read);
            again.message.body = None;
            again.poll = Some(crate::history::import::ImportPoll {
                options: vec!["A".into(), "B".into()],
                allow_multiple: false,
                ended: false,
                votes: vec![("bob".into(), 2, vec![0])],
            });
            db.import_history(&batch_with(vec![again])).await.unwrap();
            let p = db.poll(&key("alice", 5)).await.unwrap().unwrap();
            assert_eq!(
                p.options.iter().map(|o| o.voters.clone()).collect::<Vec<_>>(),
                vec![vec![], vec!["bob".to_string()]]
            );
        }

        #[tokio::test]
        async fn a_second_import_changes_nothing() {
            let db = db().await;
            let batch = || {
                let mut m = imported("c:alice", "alice", 5, "hello", MessageStatus::Read);
                m.reactions = vec![("alice".into(), "👍".into())];
                m.attachments = vec![ImportAttachment {
                    position: 0,
                    content_type: "image/png".into(),
                    file_name: None,
                    size: 9,
                    width: None,
                    height: None,
                    is_voice_note: false,
                    pointer: Some(vec![1, 2]),
                }];
                HistoryBatch {
                    conversations: vec![ImportConversation {
                        id: "c:alice".into(),
                        title: "Alice".into(),
                        expire_timer: 60,
                        ..Default::default()
                    }],
                    messages: vec![m],
                    events: vec![ImportEvent {
                        conversation_id: "c:alice".into(),
                        at: 4,
                        author: "alice".into(),
                        value: 60,
                    }],
                    default_chat_color: None,
                    default_wallpaper: None,
                    defaults_read: false,
                }
            };
            db.import_history(&batch()).await.unwrap();
            db.import_history(&batch()).await.unwrap();
            let c = conv(&db, "c:alice").await;
            assert_eq!((c.title.as_str(), c.unread_count), ("Alice", 0));
            let events = db.chat_events("c:alice", 0).await.unwrap();
            assert_eq!(events.len(), 1, "no duplicate timer event");
            assert_eq!(db.reactions(&key("alice", 5)).await.unwrap().len(), 1);
            assert_eq!(db.attachments(&key("alice", 5)).await.unwrap().len(), 1);
        }

        #[tokio::test]
        async fn import_updates_summaries_and_unread() {
            let db = db().await;
            db.import_history(&batch_with(vec![
                imported("c:alice", "alice", 5, "first", MessageStatus::Delivered),
                imported("c:alice", "alice", 9, "newest", MessageStatus::Delivered),
            ]))
            .await
            .unwrap();
            let c = conv(&db, "c:alice").await;
            assert_eq!(
                (c.unread_count, c.last_message_preview.as_str(), c.last_message_at),
                (2, "newest", 9)
            );
        }

        #[tokio::test]
        async fn an_imported_title_never_overwrites_a_known_one() {
            let db = db().await;
            alice(&db).await;
            db.import_history(&HistoryBatch {
                conversations: vec![
                    ImportConversation {
                        id: "c:alice".into(),
                        title: "Imported".into(),
                        expire_timer: 0,
                        ..Default::default()
                    },
                    ImportConversation {
                        id: "c:bob".into(),
                        title: "Bob".into(),
                        expire_timer: 30,
                        ..Default::default()
                    },
                ],
                messages: vec![imported("c:bob", "bob", 2, "hi", MessageStatus::Read)],
                events: vec![],
                default_chat_color: None,
                default_wallpaper: None,
                defaults_read: false,
            })
            .await
            .unwrap();
            assert_eq!(
                db.title("c:alice").await.unwrap().as_deref(),
                Some("Alice"),
                "known title kept"
            );
            let bob = conv(&db, "c:bob").await;
            assert_eq!(
                (bob.title.as_str(), bob.expire_timer),
                ("Bob", 30),
                "new chat filled in"
            );
        }

        #[tokio::test]
        async fn a_chat_shows_its_own_colour_else_the_phones_default() {
            let db = db().await;
            let crimson = crate::projection::chat_colors::preset(2).unwrap();
            let sea = crate::projection::chat_colors::preset(21).unwrap();
            let teal = crate::projection::chat_colors::preset(7).unwrap();
            db.import_history(&batch_with(vec![imported(
                "c:alice",
                "alice",
                1,
                "hi",
                MessageStatus::Read,
            )]))
            .await
            .unwrap();
            assert_eq!(conv(&db, "c:alice").await.chat_color, None, "nothing set: the app's");
            db.import_history(&HistoryBatch {
                conversations: vec![
                    ImportConversation {
                        id: "c:alice".into(),
                        chat_color: Some(sea.clone()),
                        ..Default::default()
                    },
                    ImportConversation {
                        id: "c:bob".into(),
                        ..Default::default()
                    },
                ],
                messages: vec![
                    imported("c:alice", "alice", 2, "hi", MessageStatus::Read),
                    imported("c:bob", "bob", 3, "hi", MessageStatus::Read),
                ],
                events: vec![],
                default_chat_color: Some(crimson.clone()),
                default_wallpaper: None,
                defaults_read: true,
            })
            .await
            .unwrap();
            assert_eq!(conv(&db, "c:alice").await.chat_color, Some(sea));
            assert_eq!(
                conv(&db, "c:bob").await.chat_color,
                Some(crimson.clone()),
                "the default"
            );
            assert!(db.set_chat_color("c:bob", Some(&teal)).await.unwrap());
            assert_eq!(conv(&db, "c:bob").await.chat_color, Some(teal));
            assert!(db.set_chat_color("c:alice", None).await.unwrap());
            assert_eq!(
                conv(&db, "c:alice").await.chat_color,
                Some(crimson),
                "back to the default"
            );
            assert!(!db.set_chat_color("c:nobody", None).await.unwrap());
        }

        #[tokio::test]
        async fn a_chat_shows_its_own_wallpaper_else_the_phones_default() {
            let db = db().await;
            let navy = crate::projection::chat_colors::wallpaper(8).unwrap();
            let peach = crate::projection::chat_colors::wallpaper(21).unwrap();
            db.import_history(&batch_with(vec![imported(
                "c:alice",
                "alice",
                1,
                "hi",
                MessageStatus::Read,
            )]))
            .await
            .unwrap();
            assert_eq!(conv(&db, "c:alice").await.wallpaper, None, "none set: none");
            db.import_history(&HistoryBatch {
                conversations: vec![ImportConversation {
                    id: "c:bob".into(),
                    wallpaper: Some(peach.clone()),
                    ..Default::default()
                }],
                messages: vec![imported("c:bob", "bob", 2, "hi", MessageStatus::Read)],
                default_wallpaper: Some(navy.clone()),
                defaults_read: true,
                ..Default::default()
            })
            .await
            .unwrap();
            assert_eq!(conv(&db, "c:alice").await.wallpaper, Some(navy.clone()), "the default");
            assert_eq!(conv(&db, "c:bob").await.wallpaper, Some(peach));
            assert!(db.set_wallpaper("c:bob", None).await.unwrap());
            assert_eq!(conv(&db, "c:bob").await.wallpaper, Some(navy.clone()));
            assert!(!db.set_wallpaper("c:nobody", None).await.unwrap());
            // a later batch that didn't read the defaults leaves them
            db.import_history(&batch_with(vec![])).await.unwrap();
            assert_eq!(conv(&db, "c:alice").await.wallpaper, Some(navy));
            // imported again, the phone has no default any more: none here either
            db.import_history(&HistoryBatch {
                defaults_read: true,
                ..Default::default()
            })
            .await
            .unwrap();
            assert_eq!(conv(&db, "c:alice").await.wallpaper, None);
        }

        #[tokio::test]
        async fn the_import_sets_the_chat_flags_and_a_later_import_resets_them() {
            let db = db().await;
            db.insert_message(&msg("alice", 1, "a")).await.unwrap();
            db.set_pinned("c:alice", true).await.unwrap();
            let flagged = |archived, pinned_order, mute_until, marked_unread| HistoryBatch {
                conversations: vec![ImportConversation {
                    id: "c:alice".into(),
                    title: "Alice".into(),
                    expire_timer: 0,
                    archived,
                    pinned_order,
                    mute_until,
                    marked_unread,
                    chat_color: None,
                    wallpaper: None,
                }],
                ..Default::default()
            };
            // our own message in the backup doesn't unarchive (only a live one does)
            let mut batch = flagged(true, None, 5_000, true);
            let mut mine = imported("c:alice", "me", 2, "mine", MessageStatus::Sent);
            mine.message.is_outgoing = true;
            batch.messages.push(mine);
            db.import_history(&batch).await.unwrap();
            let c = conv(&db, "c:alice").await;
            assert_eq!(
                (c.archived, c.pinned_order, c.mute_until, c.marked_unread),
                (true, None, 5_000, true)
            );
            db.import_history(&flagged(false, Some(2), 0, false)).await.unwrap();
            let c = conv(&db, "c:alice").await;
            assert_eq!(
                (c.archived, c.pinned_order, c.mute_until, c.marked_unread),
                (false, Some(2), 0, false)
            );
        }

        #[tokio::test]
        async fn imported_reactions_do_not_clobber_live_ones() {
            let db = db().await;
            db.insert_message(&in_conv("c:alice", msg("alice", 5, "hi")))
                .await
                .unwrap();
            db.apply(&Op::React {
                target: key("alice", 5),
                reactor: "bob".into(),
                emoji: Some("❤️".into()),
            })
            .await
            .unwrap();
            let mut m = imported("c:alice", "alice", 5, "hi", MessageStatus::Read);
            m.reactions = vec![("bob".into(), "👍".into())];
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let reactions = db.reactions(&key("alice", 5)).await.unwrap();
            assert_eq!(reactions.len(), 1);
            assert_eq!(reactions[0].emoji, "❤️", "the live reaction wins");
        }

        #[tokio::test]
        async fn unavailable_files_are_failed_rows() {
            let db = db().await;
            let mut m = imported("c:alice", "alice", 5, "files", MessageStatus::Read);
            m.attachments = vec![
                ImportAttachment {
                    position: 0,
                    content_type: "image/png".into(),
                    file_name: Some("a.png".into()),
                    size: 9,
                    width: Some(4),
                    height: Some(4),
                    is_voice_note: false,
                    pointer: Some(vec![1, 2, 3]),
                },
                ImportAttachment {
                    position: 1,
                    content_type: "image/jpeg".into(),
                    file_name: None,
                    size: 10,
                    width: None,
                    height: None,
                    is_voice_note: false,
                    pointer: None,
                },
            ];
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let files = db.attachments(&key("alice", 5)).await.unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(files[0].state, AttachmentState::Pending, "downloadable");
            assert_eq!(files[1].state, AttachmentState::Failed, "no longer available");
        }

        /// 100k messages across 50 chats; run with
        /// `cargo test -p sig-core --release -- --ignored volume` and put time and memory in the PR.
        #[tokio::test]
        #[ignore]
        async fn volume_100k_messages() {
            let db = db().await;
            let conv_id = |n: usize| format!("c:{:08}-0000-0000-0000-000000000000", n % 50);
            let started = std::time::Instant::now();
            for page in 0..100usize {
                let messages = (0..1000usize)
                    .map(|i| {
                        let n = page * 1000 + i;
                        imported(
                            &conv_id(n),
                            "alice",
                            n as i64 + 1,
                            "hello world, this is a message",
                            MessageStatus::Read,
                        )
                    })
                    .collect();
                db.import_history(&batch_with(messages)).await.unwrap();
            }
            eprintln!("100k messages imported in {:?}", started.elapsed());
            assert_eq!(db.conversations().await.unwrap().len(), 50);
        }

        /// Live clients target an edited message by any revision's timestamp: the import maps
        /// them all to the original, or reactions and edits after the import would miss it.
        #[tokio::test]
        async fn live_ops_target_imported_edits_by_any_revision() {
            let db = db().await;
            let mut m = imported("c:alice", "alice", 1_000, "newest", MessageStatus::Read);
            m.revisions = vec![2_000, 3_000];
            m.edited_at = 3_000;
            m.message.edited = true;
            db.import_history(&batch_with(vec![m])).await.unwrap();
            // a live reaction names the newest revision's timestamp
            db.apply(&Op::React {
                target: key("alice", 3_000),
                reactor: "bob".into(),
                emoji: Some("👍".into()),
            })
            .await
            .unwrap();
            assert_eq!(db.reactions(&key("alice", 1_000)).await.unwrap().len(), 1);
            // and a live edit after the import still wins
            db.apply(&Op::Edit {
                target: key("alice", 3_000),
                new_body: Some("even newer".into()),
                new_ranges: vec![],
                edited_at: 4_000,
            })
            .await
            .unwrap();
            let stored = db.message(&key("alice", 1_000)).await.unwrap().unwrap();
            assert_eq!(stored.body.as_deref(), Some("even newer"));
        }

        /// The importer and the live projector must produce the same keys, or a relink
        /// duplicates every conversation and message instead of merging.
        #[tokio::test]
        async fn imported_and_live_keys_agree() {
            use presage::libsignal_service::content::{Content, ContentBody, Metadata};
            use presage::libsignal_service::prelude::Uuid;
            use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
            use presage::proto::DataMessage;

            const ME: &str = "11111111-1111-1111-1111-111111111111";
            const ALICE: &str = "22222222-2222-2222-2222-222222222222";
            let db = db().await;
            // the live path: one incoming message from Alice, projected and applied
            let alice = ServiceId::from(Aci::from(Uuid::parse_str(ALICE).unwrap()));
            let content = Content {
                metadata: Metadata {
                    sender: alice,
                    destination: alice,
                    sender_device: DeviceId::new(1).unwrap(),
                    pni_verified: None,
                    client_timestamp: chrono::DateTime::from_timestamp_millis(5_000).unwrap(),
                    server_timestamp: chrono::DateTime::from_timestamp_millis(5_000).unwrap(),
                    needs_receipt: false,
                    unidentified_sender: false,
                    was_plaintext: false,
                    server_guid: None,
                },
                body: ContentBody::DataMessage(DataMessage {
                    timestamp: Some(5_000),
                    body: Some("live".into()),
                    ..Default::default()
                }),
            };
            for op in crate::projection::project(&content, ME) {
                db.apply(&op).await.unwrap();
            }
            // the import path: the same logical message out of an archive
            use libsignal_message_backup::proto::backup;
            let frame = |item| backup::Frame {
                item: Some(item),
                ..Default::default()
            };
            let mut importer = crate::history::import::Importer::new(ME, 1_000_000_000);
            importer.add(frame(backup::frame::Item::Recipient(backup::Recipient {
                id: 1,
                destination: Some(backup::recipient::Destination::Contact(backup::Contact {
                    aci: Some(Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec()),
                    ..Default::default()
                })),
                ..Default::default()
            })));
            importer.add(frame(backup::frame::Item::Chat(backup::Chat {
                id: 10,
                recipientId: 1,
                ..Default::default()
            })));
            importer.add(frame(backup::frame::Item::ChatItem(backup::ChatItem {
                chatId: 10,
                authorId: 1,
                dateSent: 5_000,
                directionalDetails: Some(backup::chat_item::DirectionalDetails::Incoming(
                    backup::chat_item::IncomingMessageDetails {
                        dateReceived: 5_000,
                        read: true,
                        ..Default::default()
                    },
                )),
                item: Some(backup::chat_item::Item::StandardMessage(backup::StandardMessage {
                    text: protobuf::MessageField::some(backup::Text {
                        body: "archived".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
                ..Default::default()
            })));
            let (batch, _, _) = importer.finish();
            db.import_history(&batch).await.unwrap();
            let conversations = db.conversations().await.unwrap();
            assert_eq!(conversations.len(), 1, "one chat, not a duplicate: {conversations:?}");
            assert_eq!(conversations[0].id, format!("c:{ALICE}"));
            let page = db.messages(&format!("c:{ALICE}"), None, 10).await.unwrap();
            assert_eq!(page.len(), 1, "one message, not a duplicate");
            assert_eq!(page[0].body.as_deref(), Some("live"), "what was here is kept");
        }

        /// A message deleted here must not grow imported files, reactions or previews: the
        /// archive predates the deletion.
        #[tokio::test]
        async fn a_deleted_message_gains_no_imported_content() {
            let db = db().await;
            let mut gone = in_conv("c:alice", msg("alice", 5, ""));
            gone.deleted = true;
            gone.body = None;
            db.insert_message(&gone).await.unwrap();
            let mut m = imported("c:alice", "alice", 5, "resurrected", MessageStatus::Read);
            m.reactions = vec![("bob".into(), "👍".into())];
            m.attachments = vec![ImportAttachment {
                position: 0,
                content_type: "image/png".into(),
                file_name: None,
                size: 9,
                width: None,
                height: None,
                is_voice_note: false,
                pointer: Some(vec![1, 2]),
            }];
            m.preview = Some(crate::history::import::ImportPreview {
                url: "https://example.com".into(),
                title: Some("t".into()),
                description: None,
                image: None,
            });
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let stored = db.message(&key("alice", 5)).await.unwrap().unwrap();
            assert!(stored.deleted);
            assert_eq!(stored.body, None);
            assert!(db.reactions(&key("alice", 5)).await.unwrap().is_empty());
            assert!(db.attachments(&key("alice", 5)).await.unwrap().is_empty());
            assert!(db.link_preview(&key("alice", 5)).await.unwrap().is_none());
        }

        /// Raising a disappearing message to Read must not leave its timer unstarted: the
        /// archive's expiry fills a missing one, and an earlier one is never extended.
        #[tokio::test]
        async fn an_imported_read_status_starts_a_missing_timer() {
            let db = db().await;
            let mut live = in_conv("c:alice", msg("alice", 5, "vanishes"));
            live.expires_in = 60;
            db.insert_message(&live).await.unwrap();
            let mut m = imported("c:alice", "alice", 5, "vanishes", MessageStatus::Read);
            m.message.expires_in = 60;
            m.message.expires_at = Some(99_000);
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let stored = db.message(&key("alice", 5)).await.unwrap().unwrap();
            assert_eq!(
                (stored.status, stored.expires_at),
                (MessageStatus::Read, Some(99_000)),
                "the archive's timer start fills the gap"
            );
            // an already ticking timer is never pushed later
            let mut earlier = in_conv("c:alice", msg("alice", 6, "soon"));
            earlier.expires_in = 60;
            earlier.expires_at = Some(50_000);
            db.insert_message(&earlier).await.unwrap();
            let mut m = imported("c:alice", "alice", 6, "soon", MessageStatus::Read);
            m.message.expires_in = 60;
            m.message.expires_at = Some(80_000);
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let stored = db.message(&key("alice", 6)).await.unwrap().unwrap();
            assert_eq!(stored.expires_at, Some(50_000), "the earlier expiry wins");
        }

        #[tokio::test]
        async fn an_imported_deleted_message_stores_no_text() {
            let db = db().await;
            let mut m = imported("c:alice", "alice", 5, "secret", MessageStatus::Read);
            m.message.deleted = true;
            db.import_history(&batch_with(vec![m])).await.unwrap();
            let got = db.message(&key("alice", 5)).await.unwrap().unwrap();
            assert!(got.deleted);
            assert_eq!(got.body, None);
        }
    }

    /// A temp file path removed on drop, even if the test panics.
    struct TempDb(std::path::PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("hush-{tag}-{}.db", std::process::id()));
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
                ranges: vec![],
                text: Some("q".into()),
            }),
            ..msg("me", 10, "hi")
        };
        let no_quote_text = Message {
            quote: Some(Quote {
                author: "bob".into(),
                sent_at: 8,
                ranges: vec![],
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
                ranges: vec![],
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
    async fn chat_flags_are_off_until_set() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (c.archived, c.pinned_order, c.mute_until, c.marked_unread),
            (false, None, 0, false)
        );
        db.set_archived("c:alice", true).await.unwrap();
        db.set_mute_until("c:alice", i64::MAX).await.unwrap();
        db.set_marked_unread("c:alice", true).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.archived, c.mute_until, c.marked_unread), (true, i64::MAX, true));
        db.set_archived("c:alice", false).await.unwrap();
        db.set_mute_until("c:alice", 0).await.unwrap();
        db.set_marked_unread("c:alice", false).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.archived, c.mute_until, c.marked_unread), (false, 0, false));
    }

    #[tokio::test]
    async fn pinned_chats_come_first_in_the_order_they_were_pinned() {
        let db = db().await;
        for (conv_id, author, t) in [("c:alice", "alice", 3), ("c:bob", "bob", 2), ("c:carol", "carol", 1)] {
            db.insert_message(&in_conv(conv_id, msg(author, t, "x"))).await.unwrap();
        }
        assert!(db.set_pinned("c:carol", true).await.unwrap());
        assert!(db.set_pinned("c:bob", true).await.unwrap());
        let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c:carol", "c:bob", "c:alice"]);
        assert_eq!(conv(&db, "c:carol").await.pinned_order, Some(1));
        assert!(db.set_pinned("c:carol", false).await.unwrap());
        let ids: Vec<_> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c:bob", "c:alice", "c:carol"]);
        assert_eq!(conv(&db, "c:carol").await.pinned_order, None);
    }

    #[tokio::test]
    async fn at_most_four_chats_are_pinned() {
        let db = db().await;
        for (i, name) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            db.insert_message(&in_conv(&format!("c:{name}"), msg(name, i as i64 + 1, "x")))
                .await
                .unwrap();
        }
        for name in ["a", "b", "c", "d"] {
            assert!(db.set_pinned(&format!("c:{name}"), true).await.unwrap());
        }
        assert!(!db.set_pinned("c:e", true).await.unwrap());
        assert_eq!(conv(&db, "c:e").await.pinned_order, None);
        // pinning a pinned chat keeps its place, and unpinning makes room
        assert!(db.set_pinned("c:a", true).await.unwrap());
        assert_eq!(conv(&db, "c:a").await.pinned_order, Some(1));
        assert!(db.set_pinned("c:a", false).await.unwrap());
        assert!(db.set_pinned("c:e", true).await.unwrap());
    }

    #[tokio::test]
    async fn pinning_takes_a_chat_out_of_the_archive() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.set_archived("c:alice", true).await.unwrap();
        db.set_pinned("c:alice", true).await.unwrap();
        assert!(!conv(&db, "c:alice").await.archived);
    }

    #[tokio::test]
    async fn our_own_message_unarchives_a_chat_but_an_incoming_one_does_not() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.set_archived("c:alice", true).await.unwrap();
        db.insert_message(&msg("alice", 2, "b")).await.unwrap();
        assert!(conv(&db, "c:alice").await.archived);
        db.insert_message(&Message {
            is_outgoing: true,
            ..msg("me", 3, "c")
        })
        .await
        .unwrap();
        assert!(!conv(&db, "c:alice").await.archived);
    }

    #[tokio::test]
    async fn clearing_marked_unread_says_whether_it_was_set() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        assert!(!db.clear_marked_unread("c:alice").await.unwrap());
        db.set_marked_unread("c:alice", true).await.unwrap();
        assert!(db.clear_marked_unread("c:alice").await.unwrap());
        assert!(!conv(&db, "c:alice").await.marked_unread);
    }

    #[tokio::test]
    async fn archiving_a_pinned_chat_unpins_it() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "b"))).await.unwrap();
        db.set_pinned("c:alice", true).await.unwrap();
        db.set_pinned("c:bob", true).await.unwrap();
        db.set_archived("c:alice", true).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.archived, c.pinned_order), (true, None));
        // the other pin keeps its place, and unarchiving doesn't bring the pin back
        assert_eq!(conv(&db, "c:bob").await.pinned_order, Some(2));
        db.set_archived("c:alice", false).await.unwrap();
        assert_eq!(conv(&db, "c:alice").await.pinned_order, None);
    }

    #[tokio::test]
    async fn a_chat_is_muted_while_its_time_has_not_come() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        assert!(!db.is_muted("c:alice", 5_000).await.unwrap());
        db.set_mute_until("c:alice", 5_000).await.unwrap();
        assert!(!db.is_muted("c:alice", 5_000).await.unwrap());
        assert!(db.is_muted("c:alice", 4_999).await.unwrap());
        db.set_mute_until("c:alice", i64::MAX).await.unwrap();
        assert!(db.is_muted("c:alice", i64::MAX - 1).await.unwrap());
        assert!(!db.is_muted("c:nobody", 0).await.unwrap());
    }

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
        db.replace_storage_items(&[StorageItemRow {
            key: vec![1; 16],
            kind: 4,
            conversation_id: None,
        }])
        .await
        .unwrap();
        // the pending pin change is ours, not the old manifest's: it survives
        db.reset_storage_state().await.unwrap();
        let reset = db.storage_state().await.unwrap().unwrap();
        assert_eq!(
            (reset.version, reset.account_dirty, reset.key_fingerprint.len()),
            (0, true, 0)
        );
        assert!(db.storage_items().await.unwrap().is_empty());
        db.set_account_dirty(false).await.unwrap();
        db.reset_storage_state().await.unwrap();
        assert_eq!(db.storage_state().await.unwrap(), None);
    }

    #[tokio::test]
    async fn storage_items_are_replaced_as_a_set() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        let row = |k: u8, kind, conv: Option<&str>| StorageItemRow {
            key: vec![k; 16],
            kind,
            conversation_id: conv.map(String::from),
        };
        db.replace_storage_items(&[row(1, 1, Some("c:alice")), row(2, 4, None)])
            .await
            .unwrap();
        let rows = db.storage_items().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].conversation_id.as_deref(), Some("c:alice"));
        db.replace_storage_items(&[row(3, 3, None)]).await.unwrap();
        let keys: Vec<u8> = db.storage_items().await.unwrap().iter().map(|r| r.key[0]).collect();
        assert_eq!(keys, vec![3]);
    }

    #[tokio::test]
    async fn remote_flags_apply_to_a_chat_and_create_it_if_needed() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.set_archived("c:alice", true).await.unwrap(); // dirty now
        let flags = ChatFlags {
            archived: false,
            marked_unread: true,
            mute_until: 5,
            blocked: true,
            whitelisted: true,
        };
        assert!(db.apply_remote_chat("c:alice", &flags).await.unwrap());
        let c = conv(&db, "c:alice").await;
        assert_eq!(
            (c.archived, c.marked_unread, c.mute_until, c.blocked, c.whitelisted),
            (false, true, 5, true, true)
        );
        assert!(
            db.dirty_chats().await.unwrap().is_empty(),
            "remote wins and clears the mark"
        );
        assert!(
            !db.apply_remote_chat("c:alice", &flags).await.unwrap(),
            "nothing changed the second time"
        );
        // a chat we have no messages with: created, not listed
        let archived = ChatFlags {
            archived: true,
            ..Default::default()
        };
        db.apply_remote_chat("c:bob", &archived).await.unwrap();
        assert!(db.has_conversation("c:bob").await.unwrap());
        assert!(db.conversations().await.unwrap().iter().all(|c| c.id != "c:bob"));
        // a remote archive unpins
        db.set_pinned("c:alice", true).await.unwrap();
        db.apply_remote_chat("c:alice", &archived).await.unwrap();
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
        db.apply_remote_pins(&["c:carol".into(), "c:bob".into(), "g:ff".into()])
            .await
            .unwrap();
        assert_eq!(conv(&db, "c:alice").await.pinned_order, None);
        let carol = conv(&db, "c:carol").await;
        assert_eq!(
            (carol.pinned_order, carol.archived),
            (Some(1), false),
            "a remote pin unarchives"
        );
        assert_eq!(conv(&db, "c:bob").await.pinned_order, Some(2));
        assert!(
            db.has_conversation("g:ff").await.unwrap(),
            "an unknown pinned group is created"
        );
    }

    #[tokio::test]
    async fn local_flag_changes_mark_the_chat_or_the_account_dirty() {
        let db = db().await;
        // as a push does: clears what it wrote, as it is now
        async fn clear(db: &ProjectionDb) {
            let flags = db.chat_flags("c:alice").await.unwrap().unwrap();
            db.clear_dirty(&[("c:alice".into(), flags)]).await.unwrap();
        }
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        assert!(db.dirty_chats().await.unwrap().is_empty());
        db.set_mute_until("c:alice", 9).await.unwrap();
        let dirty = db.dirty_chats().await.unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!((dirty[0].0.as_str(), dirty[0].1.mute_until), ("c:alice", 9));
        clear(&db).await;
        assert!(db.dirty_chats().await.unwrap().is_empty());
        assert_eq!(db.storage_state().await.unwrap(), None);
        db.set_pinned("c:alice", true).await.unwrap();
        assert!(db.storage_state().await.unwrap().unwrap().account_dirty);
        db.set_account_dirty(false).await.unwrap();
        assert!(!db.storage_state().await.unwrap().unwrap().account_dirty);
        db.set_archived("c:alice", true).await.unwrap(); // archiving unpins: the account changes too
        assert!(db.storage_state().await.unwrap().unwrap().account_dirty);
        // opening a chat marked unread is a change the phone should learn about
        clear(&db).await;
        db.set_marked_unread("c:alice", true).await.unwrap();
        clear(&db).await;
        assert!(db.clear_marked_unread("c:alice").await.unwrap());
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn a_mark_is_cleared_only_while_the_flags_are_as_written() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.set_mute_until("c:alice", 9).await.unwrap();
        let written = db.dirty_chats().await.unwrap();
        db.set_archived("c:alice", true).await.unwrap(); // while the write is in flight
        db.clear_dirty(&written).await.unwrap();
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1);
        // the account: a pin change after the count was read keeps the mark
        db.set_pinned("c:alice", true).await.unwrap();
        let seen = db.account_changes().await;
        db.set_pinned("c:alice", false).await.unwrap();
        db.clear_account_dirty(seen).await.unwrap();
        assert!(db.storage_state().await.unwrap().unwrap().account_dirty);
        let seen = db.account_changes().await;
        db.clear_account_dirty(seen).await.unwrap();
        assert!(!db.storage_state().await.unwrap().unwrap().account_dirty);
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
    async fn styles_and_mentions_are_kept_replaced_by_edits_and_gone_with_a_delete() {
        let db = db().await;
        let bold = |start, length| BodyRange {
            start,
            length,
            style: TextStyle::Bold,
        };
        let mention = BodyRange {
            start: 0,
            length: 1,
            style: TextStyle::Mention { aci: "bob".into() },
        };
        let m = Message {
            ranges: vec![mention.clone(), bold(2, 3)],
            quote: Some(Quote {
                author: "me".into(),
                sent_at: 1,
                text: Some("orig".into()),
                ranges: vec![bold(0, 4)],
            }),
            ..msg("alice", 2, "\u{fffc} hey")
        };
        db.insert_message(&m).await.unwrap();
        assert_eq!(db.messages("c:alice", None, 10).await.unwrap(), vec![m.clone()]);
        assert_eq!(conv(&db, "c:alice").await.last_message_ranges, m.ranges);

        db.apply(&Op::Edit {
            target: k("alice", 2),
            new_body: Some("hey".into()),
            new_ranges: vec![bold(0, 3)],
            edited_at: 3,
        })
        .await
        .unwrap();
        let edited = db.message(&k("alice", 2)).await.unwrap().unwrap();
        assert_eq!(edited.ranges, vec![bold(0, 3)]);
        assert_eq!(conv(&db, "c:alice").await.last_message_ranges, vec![bold(0, 3)]);

        db.apply(&Op::Delete { target: k("alice", 2) }).await.unwrap();
        let deleted = db.message(&k("alice", 2)).await.unwrap().unwrap();
        assert_eq!((deleted.ranges, deleted.quote.unwrap().ranges), (vec![], vec![]));
    }

    #[tokio::test]
    async fn a_groups_members_are_replaced_whole() {
        let db = db().await;
        let g = format!("g:{}", "ab".repeat(32));
        assert_eq!(db.group_members(&g).await.unwrap(), Vec::<String>::new());
        db.set_group_members(&g, &["bob".into(), "alice".into()]).await.unwrap();
        assert_eq!(db.group_members(&g).await.unwrap(), vec!["alice", "bob"]);
        db.set_group_members(&g, &["carol".into()]).await.unwrap();
        assert_eq!(db.group_members(&g).await.unwrap(), vec!["carol"]);
    }

    #[tokio::test]
    async fn opened_view_once_media_is_marked_viewed_and_its_files_go() {
        let db = db().await;
        let m = Message {
            kind: MessageKind::ViewOnce { viewed: false },
            body: None,
            ..msg("alice", 1, "")
        };
        db.insert_message(&m).await.unwrap();
        db.apply(&add(k("alice", 1), vec![new_attachment("image/jpeg", 10, false)], None))
            .await
            .unwrap();
        assert_eq!(db.attachments(&k("alice", 1)).await.unwrap().len(), 1);
        assert_eq!(
            db.apply(&Op::ViewOnceOpened { target: k("alice", 1) }).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        let opened = db.message(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!(opened.kind, MessageKind::ViewOnce { viewed: true });
        assert!(db.attachments(&k("alice", 1)).await.unwrap().is_empty());
        // presage's copy holds the media's key and the caption: it goes too
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 1)]
        );
        // again, or on a message that isn't view-once: nothing
        assert!(db
            .apply(&Op::ViewOnceOpened { target: k("alice", 1) })
            .await
            .unwrap()
            .is_empty());
        db.insert_message(&msg("alice", 2, "hi")).await.unwrap();
        assert!(db
            .apply(&Op::ViewOnceOpened { target: k("alice", 2) })
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            db.message(&k("alice", 2)).await.unwrap().unwrap().kind,
            MessageKind::Normal
        );
        assert_eq!(db.presage_forget_queue().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn our_view_once_from_the_phone_leaves_nothing_in_presage() {
        let db = db().await;
        let ours = Message {
            kind: MessageKind::ViewOnce { viewed: true },
            body: None,
            is_outgoing: true,
            ..msg("me", 3, "")
        };
        db.apply(&Op::InsertMessage(ours)).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 3)]
        );
    }

    #[tokio::test]
    async fn a_poll_counts_each_voters_latest_vote_and_its_end_sticks() {
        let db = db().await;
        let poll = |sent_at| Message {
            kind: MessageKind::Poll {
                question: Some("Lunch?".into()),
            },
            body: None,
            ..msg("alice", sent_at, "")
        };
        let created = |sent_at, allow_multiple| Op::PollCreated {
            message: k("alice", sent_at),
            options: vec!["A".into(), "B".into(), "C".into()],
            allow_multiple,
        };
        let vote = |voter: &str, vote_count, options: Vec<u32>| Op::PollVote {
            conversation_id: "c:alice".into(),
            target: k("alice", 1),
            voter: voter.into(),
            vote_count,
            options,
        };
        db.insert_message(&poll(1)).await.unwrap();
        db.apply(&created(1, false)).await.unwrap();
        assert_eq!(
            db.apply(&vote("bob", 1, vec![0])).await.unwrap(),
            changed("c:alice", "alice", 1)
        );
        db.apply(&vote("bob", 3, vec![2])).await.unwrap();
        // an older vote arriving late doesn't count
        assert!(db.apply(&vote("bob", 2, vec![1])).await.unwrap().is_empty());
        // a single-choice poll counts the first pick; picks it doesn't have don't count
        db.apply(&vote("carol", 1, vec![0, 1])).await.unwrap();
        db.apply(&vote("dave", 1, vec![9])).await.unwrap();
        // a vote taken back
        db.apply(&vote("erin", 1, vec![1])).await.unwrap();
        db.apply(&vote("erin", 2, vec![])).await.unwrap();
        let p = db.poll(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!(
            p.options
                .iter()
                .map(|o| (o.text.as_str(), o.voters.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("A", vec!["carol".to_string()]),
                ("B", vec![]),
                ("C", vec!["bob".to_string()])
            ]
        );
        assert!(!p.allow_multiple && !p.ended);
        assert_eq!(db.my_poll_vote_count(&k("alice", 1), "bob").await.unwrap(), 3);
        let end = Op::PollEnded {
            conversation_id: "c:alice".into(),
            target: k("alice", 1),
        };
        assert_eq!(db.apply(&end).await.unwrap(), changed("c:alice", "alice", 1));
        assert!(db.poll(&k("alice", 1)).await.unwrap().unwrap().ended);

        // a vote or end from another chat, or for a poll that isn't here, is dropped
        let elsewhere = Op::PollVote {
            conversation_id: "c:mallory".into(),
            target: k("alice", 2),
            voter: "mallory".into(),
            vote_count: 1,
            options: vec![0],
        };
        db.insert_message(&poll(2)).await.unwrap();
        db.apply(&created(2, true)).await.unwrap();
        assert!(db.apply(&elsewhere).await.unwrap().is_empty());
        assert_eq!(db.my_poll_vote_count(&k("alice", 2), "mallory").await.unwrap(), 0);
        let early_end = Op::PollEnded {
            conversation_id: "c:alice".into(),
            target: k("alice", 3),
        };
        assert!(db.apply(&early_end).await.unwrap().is_empty());
        db.insert_message(&poll(3)).await.unwrap();
        db.apply(&created(3, false)).await.unwrap();
        assert!(!db.poll(&k("alice", 3)).await.unwrap().unwrap().ended);

        // deleted for everyone: nothing of it is left, and later votes find nothing to join
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        assert_eq!(db.poll(&k("alice", 1)).await.unwrap(), None);
        assert_eq!(db.my_poll_vote_count(&k("alice", 1), "bob").await.unwrap(), 0);
        assert!(db.apply(&vote("bob", 9, vec![0])).await.unwrap().is_empty());
        assert_eq!(db.my_poll_vote_count(&k("alice", 1), "bob").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_ended_poll_takes_no_votes_and_its_options_never_change() {
        let db = db().await;
        db.insert_message(&Message {
            kind: MessageKind::Poll {
                question: Some("Lunch?".into()),
            },
            body: None,
            ..msg("alice", 1, "")
        })
        .await
        .unwrap();
        let created = |options: Vec<String>| Op::PollCreated {
            message: k("alice", 1),
            options,
            allow_multiple: false,
        };
        db.apply(&created(vec!["A".into(), "B".into()])).await.unwrap();
        // the same poll again, with other options: the first stands
        assert!(db.apply(&created(vec!["Rigged".into()])).await.unwrap().is_empty());
        // a text message doesn't become one by a poll sent under its timestamp
        db.insert_message(&msg("alice", 2, "plain text")).await.unwrap();
        let onto_text = Op::PollCreated {
            message: k("alice", 2),
            options: vec!["A".into()],
            allow_multiple: false,
        };
        assert!(db.apply(&onto_text).await.unwrap().is_empty());
        assert_eq!(db.poll(&k("alice", 2)).await.unwrap(), None);
        let vote = |voter: &str| Op::PollVote {
            conversation_id: "c:alice".into(),
            target: k("alice", 1),
            voter: voter.into(),
            vote_count: 1,
            options: vec![0],
        };
        db.apply(&vote("bob")).await.unwrap();
        db.apply(&Op::PollEnded {
            conversation_id: "c:alice".into(),
            target: k("alice", 1),
        })
        .await
        .unwrap();
        assert!(db.apply(&vote("carol")).await.unwrap().is_empty());
        let p = db.poll(&k("alice", 1)).await.unwrap().unwrap();
        assert_eq!(
            p.options
                .iter()
                .map(|o| (o.text.as_str(), o.voters.clone()))
                .collect::<Vec<_>>(),
            vec![("A", vec!["bob".to_string()]), ("B", vec![])]
        );
    }

    #[tokio::test]
    async fn stories_from_accepted_chats_show_for_a_day_with_their_files() {
        let db = db().await;
        let day = 24 * 3600 * 1000;
        let story = |author: &str, conversation: &str, sent_at, text: Option<&str>| {
            Op::InsertStory(NewStory {
                conversation_id: conversation.into(),
                author: author.into(),
                sent_at,
                received_at: sent_at,
                outgoing: false,
                text: text.map(|t| StoryText {
                    text: t.into(),
                    style: 0,
                    foreground: None,
                    text_background: None,
                    background: vec![0xFF112233],
                    angle: 0,
                    link: None,
                }),
            })
        };
        // a chat with alice exists (a message from her); none with mallory
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert_eq!(
            db.apply(&story("alice", "c:alice", day, Some("hello"))).await.unwrap(),
            vec![Applied::StoriesChanged]
        );
        db.apply(&story("alice", "c:alice", day + 1, None)).await.unwrap();
        db.apply(&add(
            k("alice", day + 1),
            vec![new_attachment("image/jpeg", 10, false)],
            None,
        ))
        .await
        .unwrap();
        db.apply(&story("mallory", "c:mallory", day, Some("hi stranger")))
            .await
            .unwrap();
        let shown = db.stories(day + 10).await.unwrap();
        assert_eq!(
            shown.iter().map(|s| (s.author.as_str(), s.sent_at)).collect::<Vec<_>>(),
            vec![("alice", day), ("alice", day + 1)]
        );
        assert_eq!(shown[0].text.as_ref().unwrap().background, vec![0xFF112233]);
        // the photo story's file downloads like any other
        let file = shown[1].file.as_ref().unwrap();
        assert_eq!(file.content_type, "image/jpeg");
        assert!(db.start_download(file.id).await.unwrap().is_some());
        // seen, and a day later gone with its file
        db.story_viewed(&k("alice", day)).await.unwrap();
        // story files wait to be opened: none is fetched on its own (not from strangers either)
        assert!(!db.auto_downloads().await.unwrap().contains(&file.id));
        assert!(db.stories(day + 10).await.unwrap()[0].viewed);
        // a deleted chat takes its stories along
        db.insert_message(&msg("bob", 2, "hey")).await.unwrap();
        db.apply(&story("bob", "c:bob", day, Some("bye"))).await.unwrap();
        db.delete_chat("c:bob").await.unwrap();
        assert!(db.stories(day + 10).await.unwrap().iter().all(|s| s.author != "bob"));
        assert!(!db.expire_stories(day + 10).await.unwrap(), "none old enough");
        assert!(db.expire_stories(2 * day + 1).await.unwrap());
        assert_eq!(db.stories(2 * day + 1).await.unwrap().len(), 0);
        assert!(db.attachment(file.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_story_deleted_for_everyone_goes_with_its_file_whichever_came_first() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        let story = |sent_at| {
            Op::InsertStory(NewStory {
                conversation_id: "c:alice".into(),
                author: "alice".into(),
                sent_at,
                received_at: sent_at,
                text: None,
                outgoing: false,
            })
        };
        db.apply(&story(5)).await.unwrap();
        db.apply(&add(k("alice", 5), vec![new_attachment("image/jpeg", 10, false)], None))
            .await
            .unwrap();
        let file = db.stories(10).await.unwrap()[0].file.clone().unwrap();
        assert_eq!(
            db.apply(&Op::Delete { target: k("alice", 5) }).await.unwrap(),
            vec![Applied::StoriesChanged]
        );
        assert!(db.stories(10).await.unwrap().is_empty());
        assert!(db.attachment(file.id).await.unwrap().is_none());
        // the delete before the story
        db.apply(&Op::Delete { target: k("alice", 7) }).await.unwrap();
        assert!(db.apply(&story(7)).await.unwrap().is_empty());
        assert!(db.stories(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blocked_chats_stories_dont_show() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        db.apply(&Op::InsertStory(NewStory {
            conversation_id: "c:alice".into(),
            author: "alice".into(),
            sent_at: 5,
            received_at: 5,
            text: None,
            outgoing: false,
        }))
        .await
        .unwrap();
        db.block("c:alice").await.unwrap();
        assert!(db.stories(10).await.unwrap().is_empty());
        // ours show without any chat for them
        db.apply(&Op::InsertStory(NewStory {
            conversation_id: "c:me".into(),
            author: "me".into(),
            sent_at: 6,
            received_at: 6,
            text: None,
            outgoing: true,
        }))
        .await
        .unwrap();
        assert_eq!(
            db.stories(10)
                .await
                .unwrap()
                .iter()
                .map(|s| s.author.as_str())
                .collect::<Vec<_>>(),
            ["me"]
        );
    }

    #[tokio::test]
    async fn a_contacts_username_shows_and_a_found_one_opens_a_chat() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert!(db.set_username("c:alice", Some("alice.42")).await.unwrap());
        assert!(
            !db.set_username("c:alice", Some("alice.42")).await.unwrap(),
            "unchanged"
        );
        assert_eq!(conv(&db, "c:alice").await.username.as_deref(), Some("alice.42"));
        // found by username: listed at once, to write in
        db.open_found("c:bob", "bob.7", 50).await.unwrap();
        let bob = conv(&db, "c:bob").await;
        assert_eq!((bob.username.as_deref(), bob.last_message_at), (Some("bob.7"), 50));
        // a chat that has messages keeps its place
        db.open_found("c:alice", "alice.42", 60).await.unwrap();
        assert_eq!(conv(&db, "c:alice").await.last_message_at, 1);
    }

    #[tokio::test]
    async fn listed_chats_are_asked_for_their_picture_once_a_day() {
        let db = db().await;
        let day = 24 * 3600 * 1000;
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        db.insert_message(&in_conv("g:00", msg("bob", 2, "yo"))).await.unwrap();
        // known but not listed (no messages): not asked about; nor a blocked chat
        db.upsert_conversation_title("c:carol", "Carol").await.unwrap();
        db.insert_message(&in_conv("c:mallory", msg("mallory", 3, "spam")))
            .await
            .unwrap();
        db.block("c:mallory").await.unwrap();
        assert_eq!(
            db.avatars_to_fetch(day, 10).await.unwrap(),
            vec!["g:00", "c:alice"],
            "newest first"
        );
        assert!(db.set_avatar("c:alice", Some("/data/avatars/a"), day).await.unwrap());
        db.set_avatar("g:00", None, day).await.unwrap(); // none set
        assert_eq!(conv(&db, "c:alice").await.avatar.as_deref(), Some("/data/avatars/a"));
        assert_eq!(conv(&db, "g:00").await.avatar, None);
        assert!(
            db.avatars_to_fetch(day + 1, 10).await.unwrap().is_empty(),
            "checked today"
        );
        assert_eq!(
            db.avatars_to_fetch(2 * day + 1, 1).await.unwrap().len(),
            1,
            "a day later, as many as asked"
        );
        // unchanged: no announcement needed
        assert!(!db
            .set_avatar("c:alice", Some("/data/avatars/a"), 2 * day)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_fetched_picture_is_kept_as_a_file_and_one_taken_away_goes() {
        let files = TempFiles::new("avatars");
        let db = db().await.with_files(files.0.clone());
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert!(db.store_avatar("c:alice", Some(b"jpeg"), 1).await.unwrap());
        let path = conv(&db, "c:alice").await.avatar.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg");
        assert!(
            !db.store_avatar("c:alice", Some(b"jpeg"), 2).await.unwrap(),
            "the same picture"
        );
        // a new picture is a new file (the app caches by path), the old one goes
        assert!(db.store_avatar("c:alice", Some(b"png"), 3).await.unwrap());
        let newer = conv(&db, "c:alice").await.avatar.unwrap();
        assert_ne!(newer, path);
        assert!(!std::path::Path::new(&path).exists());
        assert!(db.store_avatar("c:alice", None, 4).await.unwrap());
        assert_eq!(conv(&db, "c:alice").await.avatar, None);
        assert!(!std::path::Path::new(&newer).exists());
    }

    #[tokio::test]
    async fn the_phones_name_for_a_contact_wins_and_its_note_shows() {
        let db = db().await;
        db.upsert_conversation_title("c:alice", "Alice Profile").await.unwrap();
        db.insert_message(&msg("alice", 1, "hi")).await.unwrap();
        assert!(db
            .set_contact_details("c:alice", Some("Ali"), Some("from work"))
            .await
            .unwrap());
        assert!(
            !db.set_contact_details("c:alice", Some("Ali"), Some("from work"))
                .await
                .unwrap(),
            "unchanged"
        );
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.title.as_str(), c.note.as_deref()), ("Ali", Some("from work")));
        assert_eq!(db.title("c:alice").await.unwrap().as_deref(), Some("Ali"));
        // taken away on the phone: the profile's name again
        db.set_contact_details("c:alice", None, None).await.unwrap();
        let c = conv(&db, "c:alice").await;
        assert_eq!((c.title.as_str(), c.note), ("Alice Profile", None));
    }

    #[tokio::test]
    async fn edit_replaces_body_keeps_latest_and_updates_search_and_preview() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "orignal typo")).await.unwrap();
        let edit = |body: &str, at| Op::Edit {
            target: k("alice", 1),
            new_body: Some(body.into()),
            new_ranges: vec![],
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
            new_ranges: vec![],
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
                ranges: vec![],
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
            new_ranges: vec![],
            edited_at: 9,
        };
        assert!(db.apply(&edit).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_deleted_message_leaves_presages_store_too_whichever_came_first() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "secret")).await.unwrap();
        db.apply(&Op::Delete { target: k("alice", 1) }).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 1)]
        );
        // the delete before the message
        db.apply(&Op::Delete { target: k("alice", 2) }).await.unwrap();
        db.insert_message(&msg("alice", 2, "secret too")).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 1), ("c:alice".to_string(), 2)]
        );
    }

    #[tokio::test]
    async fn a_deleted_edited_message_takes_its_edits_out_of_presage_too() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "secret")).await.unwrap();
        db.apply(&Op::Edit {
            target: k("alice", 1),
            new_body: Some("secret v2".into()),
            new_ranges: vec![],
            edited_at: 5,
        })
        .await
        .unwrap();
        // presage keeps each revision under its own timestamp
        db.apply(&Op::Delete { target: k("alice", 5) }).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 1), ("c:alice".to_string(), 5)]
        );
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
            new_ranges: vec![],
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
            new_ranges: vec![],
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
                ranges: vec![],
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
                ranges: vec![],
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
            new_ranges: vec![],
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
            new_ranges: vec![],
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
        assert!(db.unsent_receipts().await.unwrap().is_empty(), "the phone hears first");
        assert_eq!(db.unsynced_reads().await.unwrap().len(), 3);
        db.reads_synced(&[k("alice", 1), k("alice", 2), k("bob", 3)])
            .await
            .unwrap();
        assert!(db.unsynced_reads().await.unwrap().is_empty());
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
        let set = |read_receipts, typing_indicators, link_previews| Op::Settings {
            read_receipts,
            typing_indicators,
            link_previews,
        };
        assert!(db.apply(&set(Some(false), None, None)).await.unwrap().is_empty());
        db.apply(&set(None, Some(true), None)).await.unwrap();
        assert_eq!(
            db.settings().await.unwrap(),
            Settings {
                read_receipts: Some(false),
                typing_indicators: Some(true),
                link_previews: None,
            }
        );
        db.apply(&set(Some(true), Some(false), Some(true))).await.unwrap();
        assert_eq!(
            db.settings().await.unwrap(),
            Settings {
                read_receipts: Some(true),
                typing_indicators: Some(false),
                link_previews: Some(true),
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
            let p = std::env::temp_dir().join(format!("hush-files-{tag}-{}", std::process::id()));
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
            .insert_outgoing(&mine, &[file(&first), file(&src.0.join("gone.jpg"))], None, None)
            .await;
        assert!(result.is_err());
        assert!(db.message(&k("me", 5)).await.unwrap().is_none());
        assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 0, "no copies left behind");
        // and with the file there, everything is stored
        assert!(db.insert_outgoing(&mine, &[file(&first)], None, None).await.unwrap());
        let stored = db.attachments(&k("me", 5)).await.unwrap();
        assert_eq!((stored.len(), stored[0].size), (1, 1));
        assert_eq!(std::fs::read(stored[0].path.as_ref().unwrap()).unwrap(), b"1");
    }

    #[tokio::test]
    async fn an_unsent_sticker_keeps_its_sticker_for_retry() {
        let files = TempFiles::new("sticker");
        let src = TempFiles::new("sticker-src");
        std::fs::create_dir_all(&src.0).unwrap();
        let image = src.0.join("7");
        std::fs::write(&image, b"webp").unwrap();
        let db = db().await.with_files(files.0.clone());
        let mine = Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            body: None,
            kind: MessageKind::Sticker {
                emoji: Some("🦊".into()),
            },
            ..msg("me", 6, "")
        };
        let sticker = crate::send::SendSticker {
            pack_id: vec![1; 16],
            pack_key: vec![2; 32],
            sticker_id: 7,
            emoji: Some("🦊".into()),
        };
        let file = OutgoingFile {
            path: image.to_string_lossy().into(),
            content_type: "image/webp".into(),
            width: None,
            height: None,
            is_voice_note: false,
        };
        assert!(db.insert_outgoing(&mine, &[file], None, Some(&sticker)).await.unwrap());
        assert_eq!(db.attachments(&k("me", 6)).await.unwrap().len(), 1, "its image");
        assert_eq!(db.outgoing_sticker(&k("me", 6)).await.unwrap(), Some(sticker.clone()));
        db.forget_outgoing_sticker(&k("me", 6)).await.unwrap();
        assert_eq!(db.outgoing_sticker(&k("me", 6)).await.unwrap(), None);
        assert_eq!(db.outgoing_sticker(&k("me", 99)).await.unwrap(), None);
        // a failed sticker deleted: which sticker it was goes too
        let mut again = mine.clone();
        again.sent_at = 7;
        assert!(db.insert_outgoing(&again, &[], None, Some(&sticker)).await.unwrap());
        db.apply(&Op::Delete { target: k("me", 7) }).await.unwrap();
        assert_eq!(db.outgoing_sticker(&k("me", 7)).await.unwrap(), None);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outgoing_stickers")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn a_sent_messages_link_preview_is_kept_with_its_image() {
        let files = TempFiles::new("preview");
        let src = TempFiles::new("preview-src");
        std::fs::create_dir_all(&src.0).unwrap();
        let image = src.0.join("cover.jpg");
        std::fs::write(&image, b"jpg").unwrap();
        let db = db().await.with_files(files.0.clone());
        let mine = Message {
            is_outgoing: true,
            status: MessageStatus::Pending,
            ..msg("me", 6, "see https://example.com/a")
        };
        let preview = PreviewDraft {
            url: "https://example.com/a".into(),
            title: Some("A page".into()),
            description: Some("About it".into()),
            image: Some(OutgoingFile {
                path: image.to_string_lossy().into(),
                content_type: "image/jpeg".into(),
                width: None,
                height: None,
                is_voice_note: false,
            }),
        };
        assert!(db.insert_outgoing(&mine, &[], Some(&preview), None).await.unwrap());
        // the image is the preview's, not one of the message's files
        assert!(db.attachments(&k("me", 6)).await.unwrap().is_empty());
        let stored = db.link_preview(&k("me", 6)).await.unwrap().unwrap();
        assert_eq!(
            (
                stored.url.as_str(),
                stored.title.as_deref(),
                stored.description.as_deref()
            ),
            ("https://example.com/a", Some("A page"), Some("About it"))
        );
        let img = stored.image.unwrap();
        assert_eq!(
            (img.state, img.content_type.as_str()),
            (AttachmentState::Ready, "image/jpeg")
        );
        assert_eq!(std::fs::read(img.path.unwrap()).unwrap(), b"jpg");
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
        db.add_chat_event("c:alice", 20, ChatEventKind::DecryptionFailed, "alice", None)
            .await
            .unwrap();
        db.add_chat_event("c:alice", 10, ChatEventKind::DecryptionFailed, "alice", None)
            .await
            .unwrap();
        db.add_chat_event("c:bob", 15, ChatEventKind::DecryptionFailed, "bob", None)
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
        db.insert_message(&Message {
            ranges: vec![BodyRange {
                start: 0,
                length: 2,
                style: TextStyle::Spoiler,
            }],
            ..in_conv("c:bob", msg("bob", 50, "hi"))
        })
        .await
        .unwrap();
        db.add_chat_event("c:alice", 10, ChatEventKind::DecryptionFailed, "alice", None)
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
        db.add_chat_event("c:carol", 9, ChatEventKind::DecryptionFailed, "carol", None)
            .await
            .unwrap();
        let c = conv(&db, "c:carol").await;
        assert!(c.last_message_undecryptable);
        assert!(!c.last_message_outgoing && !c.last_message_deleted && c.last_message_file.is_none());
        db.add_chat_event("c:bob", 60, ChatEventKind::DecryptionFailed, "bob", None)
            .await
            .unwrap();
        let c = db.conversations().await.unwrap().remove(0);
        assert_eq!(c.id, "c:bob", "moved up");
        // the older message's ranges don't apply to the notice
        assert_eq!(
            (c.last_message_preview.as_str(), c.last_message_ranges),
            (UNDECRYPTABLE_PREVIEW, vec![])
        );
        // a later message takes over the preview; our own session reset doesn't count
        db.insert_message(&in_conv("c:bob", msg("bob", 70, "back")))
            .await
            .unwrap();
        db.add_chat_event("c:bob", 80, ChatEventKind::SessionReset, "bob", None)
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
        db.apply(&Op::Edit {
            target: k("alice", 1),
            new_body: Some("secret plan v2".into()),
            new_ranges: vec![],
            edited_at: 7,
        })
        .await
        .unwrap();
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
            vec![Expired {
                conversation_id: "c:alice".into(),
                key: k("alice", 1),
            }]
        );
        let mut queued = db.presage_forget_queue().await.unwrap();
        queued.sort();
        assert_eq!(
            queued,
            vec![("c:alice".to_string(), 1), ("c:alice".to_string(), 7)],
            "presage's copies of it and its edit are queued for deletion there"
        );
        db.presage_forgotten("c:alice", 1).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 7)]
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
        // messages don't change it (presage's view, by version, is synced in by the actor)
        db.insert_message(&Message {
            expires_in: 3600,
            ..msg("alice", 9, "new")
        })
        .await
        .unwrap();
        assert_eq!(db.expire_timer("c:alice").await.unwrap(), 604_800);
    }

    #[tokio::test]
    async fn a_read_on_the_phone_starts_the_timer_when_it_was_read_not_when_we_hear_of_it() {
        let db = db().await;
        let timed = |sent_at| Message {
            expires_in: 3600,
            ..msg("alice", sent_at, "x")
        };
        db.insert_message(&timed(1)).await.unwrap();
        db.apply(&Op::Read {
            targets: vec![k("alice", 1), k("alice", 2)],
            at: 1_000,
        })
        .await
        .unwrap();
        assert_eq!(
            db.message(&k("alice", 1)).await.unwrap().unwrap().expires_at,
            Some(1_000 + 3_600_000)
        );
        // read before it reached us: its timer starts from that read too
        db.insert_message(&timed(2)).await.unwrap();
        assert_eq!(
            db.message(&k("alice", 2)).await.unwrap().unwrap().expires_at,
            Some(1_000 + 3_600_000)
        );
        // a read "in the future" (a phone's clock ahead) starts no later than now
        db.insert_message(&timed(3)).await.unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        db.apply(&Op::Read {
            targets: vec![k("alice", 3)],
            at: i64::MAX / 4,
        })
        .await
        .unwrap();
        let at = db.message(&k("alice", 3)).await.unwrap().unwrap().expires_at.unwrap();
        assert!(at <= chrono::Utc::now().timestamp_millis() + 3_600_000 && at >= now + 3_600_000);
    }

    #[tokio::test]
    async fn the_timer_presage_sends_with_is_the_one_kept() {
        let db = db().await;
        db.insert_message(&Message {
            is_outgoing: true,
            ..msg("me", 10, "brb")
        })
        .await
        .unwrap();
        db.set_message_timer(&k("me", 10), 60).await.unwrap();
        let m = db.message(&k("me", 10)).await.unwrap().unwrap();
        assert_eq!(
            (m.expires_in, m.expires_at),
            (60, Some(10 + 60_000)),
            "started when sent"
        );
        db.set_message_timer(&k("me", 10), 0).await.unwrap();
        let m = db.message(&k("me", 10)).await.unwrap().unwrap();
        assert_eq!((m.expires_in, m.expires_at), (0, None));
        assert!(db.set_chat_timer("c:alice", 86_400).await.unwrap(), "changed");
        assert!(!db.set_chat_timer("c:alice", 86_400).await.unwrap(), "unchanged");
        assert_eq!(db.expire_timer("c:alice").await.unwrap(), 86_400);
        assert!(
            db.chat_events("c:alice", 0).await.unwrap().is_empty(),
            "not a change anyone made"
        );
    }

    #[tokio::test]
    async fn a_redelivered_timer_change_shows_once() {
        let db = db().await;
        let op = Op::SetTimer {
            conversation_id: "c:alice".into(),
            author: "alice".into(),
            seconds: 60,
            at: 5,
        };
        db.apply(&op).await.unwrap();
        db.apply(&op).await.unwrap();
        assert_eq!(db.chat_events("c:alice", 0).await.unwrap().len(), 1);
    }

    // ---- Blocking and message requests ----

    async fn phone_state_known(db: &ProjectionDb) {
        db.set_storage_state(&StorageState {
            version: 3,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    fn mine(conv: &str, sent_at: i64) -> Message {
        Message {
            conversation_id: conv.into(),
            is_outgoing: true,
            status: MessageStatus::Sent,
            ..msg("me", sent_at, "hi")
        }
    }

    #[tokio::test]
    async fn a_chat_is_a_request_only_once_the_phones_state_is_known() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "hello?")).await.unwrap();
        assert!(!conv(&db, "c:alice").await.message_request, "state unknown: accepted");
        phone_state_known(&db).await;
        assert!(conv(&db, "c:alice").await.message_request);
        db.accept("c:alice").await.unwrap();
        assert!(!conv(&db, "c:alice").await.message_request);
    }

    #[tokio::test]
    async fn sending_a_phone_contact_or_a_block_means_no_request() {
        let db = db().await;
        phone_state_known(&db).await;
        for (i, c) in ["c:alice", "c:bob", "c:carol"].into_iter().enumerate() {
            db.insert_message(&in_conv(c, msg("x", i as i64, "hello?")))
                .await
                .unwrap();
        }
        db.insert_message(&mine("c:alice", 9)).await.unwrap();
        db.set_phone_contact("c:bob", true).await.unwrap();
        db.block("c:carol").await.unwrap();
        for c in ["c:alice", "c:bob", "c:carol"] {
            assert!(!conv(&db, c).await.message_request, "{c}");
        }
        assert!(conv(&db, "c:carol").await.blocked);
    }

    #[tokio::test]
    async fn block_and_accept_set_the_flags_and_mark_the_chat_for_the_phone() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.accept("c:alice").await.unwrap();
        db.block("c:alice").await.unwrap();
        let dirty = db.dirty_chats().await.unwrap();
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].1.blocked && !dirty[0].1.whitelisted);
        assert!(db.is_blocked("c:alice").await.unwrap());
        assert_eq!(db.blocked_chats().await.unwrap(), vec!["c:alice".to_string()]);
        assert!(db.accept("c:alice").await.unwrap(), "it was blocked");
        let f = db.chat_flags("c:alice").await.unwrap().unwrap();
        assert!(!f.blocked && f.whitelisted);
        assert!(!db.is_blocked("c:alice").await.unwrap());
        // the write of the blocked state lands after the unblock: the chat stays dirty
        db.clear_dirty(&dirty).await.unwrap();
        assert_eq!(db.dirty_chats().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn receipts_wait_while_a_chat_is_a_request_or_blocked() {
        let db = db().await;
        phone_state_known(&db).await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "b"))).await.unwrap();
        db.accept("c:bob").await.unwrap();
        db.block("c:bob").await.unwrap();
        db.queue_receipts(&[k("alice", 1), k("bob", 2)]).await.unwrap();
        assert_eq!(
            db.unsynced_reads().await.unwrap(),
            vec![k("alice", 1), k("bob", 2)],
            "the phone still learns they were read"
        );
        db.reads_synced(&[k("alice", 1), k("bob", 2)]).await.unwrap();
        assert!(db.unsent_receipts().await.unwrap().is_empty());
        db.accept("c:alice").await.unwrap();
        db.accept("c:bob").await.unwrap();
        assert_eq!(db.unsent_receipts().await.unwrap(), vec![k("alice", 1), k("bob", 2)]);
    }

    #[tokio::test]
    async fn deleting_a_chat_removes_its_contents_and_queues_presages_copy() {
        let files = TempFiles::new("delete-chat");
        let db = db().await.with_files(files.0.clone());
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv("c:bob", msg("bob", 2, "b"))).await.unwrap();
        db.apply(&add(k("alice", 1), vec![new_attachment("image/jpeg", 3, false)], None))
            .await
            .unwrap();
        let id = db.attachments(&k("alice", 1)).await.unwrap()[0].id;
        db.start_download(id).await.unwrap();
        db.finish_download(id, Ok(b"jpg".to_vec())).await.unwrap();
        let path = db.attachment(id).await.unwrap().unwrap().path.unwrap();
        db.apply(&Op::React {
            target: k("alice", 1),
            reactor: "me".into(),
            emoji: Some("👍".into()),
        })
        .await
        .unwrap();
        db.add_chat_event("c:alice", 5, ChatEventKind::MissedCall, "alice", None)
            .await
            .unwrap();
        db.queue_receipts(&[k("alice", 1)]).await.unwrap();
        db.accept("c:alice").await.unwrap();
        db.set_pinned("c:alice", true).await.unwrap();
        db.set_account_dirty(false).await.unwrap();
        db.delete_chat("c:alice").await.unwrap();
        assert!(db.pinned_chat_ids().await.unwrap().is_empty(), "no invisible pin");
        assert!(
            db.storage_state().await.unwrap().unwrap().account_dirty,
            "the phone unpins too"
        );
        let ids: Vec<String> = db.conversations().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c:bob".to_string()], "no longer listed");
        assert!(db.message(&k("alice", 1)).await.unwrap().is_none());
        assert!(db.reactions(&k("alice", 1)).await.unwrap().is_empty());
        assert!(db.chat_events("c:alice", 0).await.unwrap().is_empty());
        assert!(db.unsent_receipts().await.unwrap().is_empty());
        assert!(db.search("a", None, 10).await.unwrap().is_empty());
        assert!(!std::path::Path::new(&path).exists(), "its files are gone");
        assert!(!db.chat_flags("c:alice").await.unwrap().unwrap().whitelisted);
        assert_eq!(db.presage_forget_threads().await.unwrap(), vec!["c:alice".to_string()]);
        db.presage_thread_forgotten("c:alice").await.unwrap();
        assert!(db.presage_forget_threads().await.unwrap().is_empty());
        // a new message brings it back, as a request
        phone_state_known(&db).await;
        db.insert_message(&msg("alice", 9, "again")).await.unwrap();
        assert!(conv(&db, "c:alice").await.message_request);
    }

    const G: &str = "g:abababababababababababababababababababababababababababababababab";

    #[tokio::test]
    async fn the_phones_blocked_list_replaces_ours() {
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv(G, msg("bob", 2, "b"))).await.unwrap();
        db.block("c:alice").await.unwrap();
        db.clear_dirty(&db.dirty_chats().await.unwrap()).await.unwrap(); // the phone has it
                                                                         // blocked here, not on the phone yet: the phone's older list can't undo it
        db.insert_message(&in_conv("c:carol", msg("carol", 3, "c")))
            .await
            .unwrap();
        db.block("c:carol").await.unwrap();
        // the list can't name a PNI chat, so it can't unblock one either
        let pni = "c:PNI:44444444-4444-4444-4444-444444444444";
        db.insert_message(&in_conv(pni, msg("pni", 4, "p"))).await.unwrap();
        db.block(pni).await.unwrap();
        db.clear_dirty(&db.dirty_chats().await.unwrap()).await.unwrap();
        db.block("c:carol").await.unwrap();
        let bob = "22222222-2222-2222-2222-222222222222";
        let op = Op::BlockedList {
            acis: vec![bob.into()],
            groups: vec![super::super::ids::group_identifier(G).unwrap()],
        };
        let applied = db.apply(&op).await.unwrap();
        assert_eq!(applied, vec![Applied::ConversationsChanged]);
        let a = db.chat_flags("c:alice").await.unwrap().unwrap();
        assert!(!a.blocked && a.whitelisted, "off the list: unblocked and accepted");
        assert!(
            db.is_blocked(&format!("c:{bob}")).await.unwrap(),
            "an unknown ACI gets a row"
        );
        assert!(db.is_blocked(G).await.unwrap(), "groups match by identifier");
        assert!(db.is_blocked("c:carol").await.unwrap(), "a block not yet written stays");
        assert!(db.is_blocked(pni).await.unwrap(), "a PNI block stays");
        assert!(
            db.dirty_chats().await.unwrap().iter().all(|(id, _)| id != G),
            "not written back"
        );
        assert!(
            db.apply(&op).await.unwrap().is_empty(),
            "nothing changed the second time"
        );
    }

    #[tokio::test]
    async fn request_responses_from_the_phone_apply_without_writing_back() {
        use crate::projection::{RequestResponse as R, RequestThread};
        let db = db().await;
        db.insert_message(&msg("alice", 1, "a")).await.unwrap();
        db.insert_message(&in_conv(G, msg("bob", 2, "b"))).await.unwrap();
        let alice = |response| Op::RequestResponse {
            thread: RequestThread::Aci("alice".into()),
            response,
        };
        db.apply(&alice(R::Accept)).await.unwrap();
        assert!(db.chat_flags("c:alice").await.unwrap().unwrap().whitelisted);
        db.apply(&alice(R::Block)).await.unwrap();
        assert!(db.is_blocked("c:alice").await.unwrap());
        db.apply(&alice(R::BlockAndDelete)).await.unwrap();
        assert!(db.message(&k("alice", 1)).await.unwrap().is_none());
        db.apply(&Op::RequestResponse {
            thread: RequestThread::Group(super::super::ids::group_identifier(G).unwrap()),
            response: R::Delete,
        })
        .await
        .unwrap();
        assert!(db.message(&k("bob", 2)).await.unwrap().is_none());
        assert!(db.dirty_chats().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dropped_message_is_queued_for_forgetting_in_presage() {
        let db = db().await;
        db.queue_presage_forget("c:alice", 9).await.unwrap();
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![("c:alice".to_string(), 9)]
        );
    }
}
