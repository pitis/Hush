use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::actor::{self, Command};
use crate::error::CoreError;
use crate::events::{from_applied, CoreEvent, EventListener};
use crate::projection::{Conversation, Message, MessageKey, MessageStatus, ProjectionDb, Quote, Reaction};
use crate::send::{self, Job, SendJob};

/// The app's handle on the core. Cheap to call from any thread; Signal work happens on the core
/// thread and reports back through the `EventListener`.
/// Keep the `EventListener` free of strong references back to the `Client`: the core thread lives
/// as long as the `Client` does.
#[derive(uniffi::Object)]
pub struct Client {
    commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    db: ProjectionDb,
    linked: Arc<AtomicBool>,
    own_aci: Arc<Mutex<Option<String>>>,
    listener: Arc<dyn EventListener>,
    /// Last `sent_at` handed out: own messages need unique timestamps (they are their identity).
    last_sent_at: Mutex<i64>,
}

impl Drop for Client {
    /// Stops the core thread and waits for it, so the data directory is free when this returns.
    fn drop(&mut self) {
        drop(self.commands.lock().ok().and_then(|mut c| c.take()));
        if let Some(t) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = t.join();
        }
    }
}

impl Client {
    fn next_sent_at(&self) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut last = self.last_sent_at.lock().unwrap_or_else(|e| e.into_inner());
        *last = now.max(*last + 1);
        *last
    }

    /// A stored message, or why there is none.
    async fn stored(&self, key: &MessageKey) -> Result<Message, CoreError> {
        self.db.message(key).await?.ok_or(CoreError::InvalidInput {
            message: "unknown message".into(),
        })
    }

    /// Queues a reaction, edit or delete of `conversation_id`'s message sent at `at`.
    fn send_mutation(
        &self,
        conversation_id: String,
        body: impl Into<presage::libsignal_service::content::ContentBody>,
        at: u64,
    ) -> Result<(), CoreError> {
        self.send(Command::Send(Job::Mutation {
            conversation_id,
            body: Box::new(body.into()),
            at,
        }))
    }

    fn send(&self, cmd: Command) -> Result<(), CoreError> {
        let commands = self.commands.lock().map_err(|_| CoreError::Closed)?;
        commands
            .as_ref()
            .ok_or(CoreError::Closed)?
            .send(cmd)
            .map_err(|_| CoreError::Closed)
    }
}

#[uniffi::export]
impl Client {
    /// Opens (creating if needed) the data in `data_dir` with the 64-hex-char `db_key_hex`.
    #[uniffi::constructor]
    pub fn open(
        data_dir: String,
        db_key_hex: String,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<Self>, CoreError> {
        crate::logging::init();
        let linked = Arc::new(AtomicBool::new(false));
        let own_aci = Arc::new(Mutex::new(None));
        let h = actor::spawn(
            PathBuf::from(data_dir),
            db_key_hex,
            listener.clone(),
            linked.clone(),
            own_aci.clone(),
        )?;
        Ok(Arc::new(Self {
            commands: Mutex::new(Some(h.commands)),
            thread: Mutex::new(Some(h.thread)),
            db: h.projection,
            linked,
            own_aci,
            listener,
            last_sent_at: Mutex::new(0),
        }))
    }

    /// Our own ACI once linked; own messages carry it as their author.
    pub fn own_aci(&self) -> Option<String> {
        self.own_aci.lock().ok().and_then(|a| a.clone())
    }

    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::SeqCst)
    }

    /// Starts linking as a secondary device: emits `LinkUrl`, then `Linked`, or `Error` and
    /// `Connection(Unlinked)` (the code expires after 5 minutes). Call `start` after `Linked`.
    pub fn link(&self, device_name: String) -> Result<(), CoreError> {
        self.send(Command::Link { device_name })
    }

    /// Starts receiving (idempotent). Emits `Connection`, `MessageChanged`, `Ready`, …
    pub fn start(&self) -> Result<(), CoreError> {
        self.send(Command::Start)
    }

    /// The user is typing in a conversation (`started`), or stopped (sent, cleared the field, left
    /// the chat). Call it on every change of the field; typing stops by itself after `TYPING_IDLE`
    /// (10 s) without one. Nothing is sent in Note to Self.
    pub fn set_typing(&self, conversation_id: String, started: bool) -> Result<(), CoreError> {
        if !self.is_linked() {
            return Err(CoreError::NotLinked);
        }
        if send::recipient(&conversation_id).is_none() {
            return Err(CoreError::InvalidInput {
                message: format!("not a conversation: {conversation_id}"),
            });
        }
        if self.own_aci().is_some_and(|me| conversation_id == format!("c:{me}")) {
            return Ok(());
        }
        self.send(Command::Typing {
            conversation_id,
            started,
        })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    /// Shows `body` at once as a pending message and queues it for sending (it moves to Sent or
    /// Failed, reported through `MessageChanged`). Returns the new message's key.
    pub async fn send_text(
        &self,
        conversation_id: String,
        body: String,
        quote: Option<Quote>,
    ) -> Result<MessageKey, CoreError> {
        if body.trim().is_empty() {
            return Err(CoreError::InvalidInput {
                message: "empty message".into(),
            });
        }
        if crate::send::recipient(&conversation_id).is_none() {
            return Err(CoreError::InvalidInput {
                message: format!("not a conversation: {conversation_id}"),
            });
        }
        let author = self.own_aci().ok_or(CoreError::NotLinked)?;
        let mut message = Message {
            conversation_id: conversation_id.clone(),
            author,
            sent_at: self.next_sent_at(),
            received_at: 0,
            body: Some(body.clone()),
            is_outgoing: true,
            status: MessageStatus::Pending,
            edited: false,
            deleted: false,
            quote: quote.clone(),
        };
        // (author, sent_at) is the identity: on a collision (a phone-sent message in the same ms,
        // a clock that went back) take the next timestamp rather than dropping the message.
        for _ in 0..1000 {
            message.received_at = message.sent_at;
            if self.db.insert_message(&message).await? {
                break;
            }
            message.sent_at = self.next_sent_at();
        }
        let key = message.key();
        self.listener.on_event(CoreEvent::MessageChanged {
            conversation_id: conversation_id.clone(),
            key: key.clone(),
        });
        let queued = self.send(Command::Send(Job::Text(SendJob {
            key: key.clone(),
            conversation_id,
            body,
            quote,
        })));
        if let Err(e) = queued {
            self.db.set_failed(&key).await?;
            return Err(e);
        }
        Ok(key)
    }

    /// Re-queues a Failed message. Returns false if it isn't Failed.
    pub async fn retry(&self, key: MessageKey) -> Result<bool, CoreError> {
        if !self.db.set_pending(&key).await? {
            return Ok(false);
        }
        let m = self.db.message(&key).await?.ok_or(CoreError::InvalidInput {
            message: "unknown message".into(),
        })?;
        self.listener.on_event(CoreEvent::MessageChanged {
            conversation_id: m.conversation_id.clone(),
            key: key.clone(),
        });
        let body = m.body.unwrap_or_default();
        self.send(Command::Send(Job::Text(SendJob {
            key,
            conversation_id: m.conversation_id,
            body,
            quote: m.quote,
        })))?;
        Ok(true)
    }

    /// Reacts to a message with `emoji`, or takes back our reaction `emoji` (`remove`). It shows
    /// here once sent (`MessageChanged`); a failed send is reported as `Error`.
    pub async fn react(&self, target: MessageKey, emoji: String, remove: bool) -> Result<(), CoreError> {
        self.own_aci().ok_or(CoreError::NotLinked)?;
        if emoji.is_empty() {
            return Err(CoreError::InvalidInput {
                message: "no emoji".into(),
            });
        }
        let m = self.stored(&target).await?;
        if m.deleted {
            return Err(CoreError::InvalidInput {
                message: "the message was deleted".into(),
            });
        }
        let at = self.next_sent_at() as u64;
        let body = send::reaction(&m.conversation_id, &target, emoji, remove, at);
        self.send_mutation(m.conversation_id, body, at)
    }

    /// Replaces the text of our own message (sent, under 24 hours old). It shows here once sent.
    pub async fn edit(&self, key: MessageKey, new_body: String) -> Result<(), CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        if new_body.trim().is_empty() {
            return Err(CoreError::InvalidInput {
                message: "empty message".into(),
            });
        }
        let m = self.stored(&key).await?;
        let at = self.next_sent_at();
        send::check_own_change(&m, &me, at).map_err(|message| CoreError::InvalidInput { message })?;
        let body = send::edit(&m, new_body, at as u64);
        self.send_mutation(m.conversation_id, body, at as u64)
    }

    /// Deletes our own message (sent, under 24 hours old) for everyone. It shows here once sent.
    pub async fn delete_for_everyone(&self, key: MessageKey) -> Result<(), CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        let m = self.stored(&key).await?;
        let at = self.next_sent_at();
        send::check_own_change(&m, &me, at).map_err(|message| CoreError::InvalidInput { message })?;
        let body = send::delete(&m.conversation_id, m.sent_at, at as u64);
        self.send_mutation(m.conversation_id, body, at as u64)
    }

    /// Marks the conversation's unread messages read here at once, then sends read receipts to
    /// their senders and tells our phone. The receipts are stored first and go out as soon as
    /// sending runs (retried until sent). Returns how many were unread.
    pub async fn mark_read(&self, conversation_id: String) -> Result<u32, CoreError> {
        let me = self.own_aci().ok_or(CoreError::NotLinked)?;
        let targets = self.db.unread(&conversation_id).await?;
        if targets.is_empty() {
            return Ok(0);
        }
        self.db.queue_receipts(&targets).await?;
        for op in send::own_ops(&me, &conversation_id, send::read_sync(&targets).into()) {
            for applied in self.db.apply(&op).await? {
                self.listener.on_event(from_applied(applied));
            }
        }
        // if the core is closing, the stored receipts go out on the next start
        let _ = self.send(Command::Send(Job::Receipts));
        Ok(targets.len() as u32)
    }

    /// The stored name of any conversation or contact (e.g. a group member's `c:<aci>`).
    pub async fn title(&self, conversation_id: String) -> Result<Option<String>, CoreError> {
        Ok(self.db.title(&conversation_id).await?)
    }

    pub async fn conversations(&self) -> Result<Vec<Conversation>, CoreError> {
        Ok(self.db.conversations().await?)
    }

    pub async fn messages(
        &self,
        conversation_id: String,
        before: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<Message>, CoreError> {
        Ok(self.db.messages(&conversation_id, before, limit).await?)
    }

    pub async fn search(&self, query: String, limit: u32) -> Result<Vec<Message>, CoreError> {
        Ok(self.db.search(&query, limit).await?)
    }

    pub async fn message(&self, key: MessageKey) -> Result<Option<Message>, CoreError> {
        Ok(self.db.message(&key).await?)
    }

    pub async fn reactions(&self, key: MessageKey) -> Result<Vec<Reaction>, CoreError> {
        Ok(self.db.reactions(&key).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ConnectionState, CoreEvent};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Sink(Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("semaphore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        TempDir(d)
    }

    fn wait_for(sink: &Sink, pred: impl Fn(&CoreEvent) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if sink.0.lock().unwrap().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn fresh_dir_is_unlinked_and_queries_work_from_another_runtime() {
        let dir = temp_dir("client-fresh");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        assert!(!client.is_linked());
        // the app calls these from UniFFI's runtime, not the core thread's
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(client.conversations().await.unwrap().is_empty());
            assert!(client.messages("c:x".into(), None, 10).await.unwrap().is_empty());
            assert!(client.search("hi".into(), 10).await.unwrap().is_empty());
        });
        assert!(dir.0.join("presage.db").exists() && dir.0.join("projection.db").exists());
    }

    #[test]
    fn start_while_unlinked_reports_unlinked() {
        let dir = temp_dir("client-start");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        client.start().unwrap();
        assert!(wait_for(&sink, |e| *e
            == CoreEvent::Connection {
                state: ConnectionState::Unlinked
            }));
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let dir = temp_dir("client-key");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        // dropping the client waits for the core thread, so the files are free right away
        drop(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap());
        assert!(Client::open(dir.0.to_string_lossy().into(), "cd".repeat(32), sink.clone()).is_err());
        assert!(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).is_ok());
    }

    #[test]
    fn a_second_client_on_the_same_data_is_refused() {
        let dir = temp_dir("client-lock");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let first = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        let second = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone());
        assert!(matches!(second, Err(CoreError::AlreadyOpen)));
        drop(first);
        assert!(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).is_ok());
    }

    #[test]
    fn unreadable_registration_fails_open_instead_of_looking_unlinked() {
        let dir = temp_dir("client-corrupt");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        drop(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap());
        // corrupt the stored registration the way a bad presage upgrade or disk error could
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opts = crate::actor::sqlite_options(&dir.0.join("presage.db"), &"ab".repeat(32)).unwrap();
            let mut conn = sqlx::ConnectOptions::connect(&opts).await.unwrap();
            sqlx::query("INSERT INTO kv(key, value) VALUES ('registration', x'00')")
                .execute(&mut conn)
                .await
                .unwrap();
        });
        let err = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink)
            .err()
            .expect("must fail");
        assert!(matches!(err, CoreError::Storage { .. }), "{err}");
    }

    #[test]
    fn sending_needs_a_link_and_valid_input() {
        let dir = temp_dir("client-send");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let to = "c:22222222-2222-2222-2222-222222222222".to_string();
            assert!(matches!(
                client.send_text(to.clone(), "  ".into(), None).await,
                Err(CoreError::InvalidInput { .. })
            ));
            assert!(matches!(
                client.send_text("c:nope".into(), "hi".into(), None).await,
                Err(CoreError::InvalidInput { .. })
            ));
            assert!(matches!(
                client.send_text(to, "hi".into(), None).await,
                Err(CoreError::NotLinked)
            ));
        });
        assert_eq!(client.own_aci(), None);
    }

    #[test]
    fn own_message_timestamps_are_unique_and_increasing() {
        let dir = temp_dir("client-ts");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let stamps: Vec<i64> = (0..100).map(|_| client.next_sent_at()).collect();
        assert!(stamps.windows(2).all(|w| w[1] > w[0]));
    }

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    /// A client that believes it is linked as `ME` (nothing is sent: receiving never starts).
    fn as_me(tag: &str) -> (TempDir, Arc<Sink>, Arc<Client>, tokio::runtime::Runtime) {
        let dir = temp_dir(tag);
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap();
        *client.own_aci.lock().unwrap() = Some(ME.into());
        client.linked.store(true, Ordering::SeqCst);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        (dir, sink, client, rt)
    }

    fn stored(author: &str, sent_at: i64) -> Message {
        Message {
            conversation_id: format!("c:{ALICE}"),
            author: author.into(),
            sent_at,
            received_at: sent_at,
            body: Some("x".into()),
            is_outgoing: author == ME,
            status: if author == ME {
                MessageStatus::Sent
            } else {
                MessageStatus::Delivered
            },
            edited: false,
            deleted: false,
            quote: None,
        }
    }

    fn invalid<T: std::fmt::Debug>(r: Result<T, CoreError>) -> bool {
        matches!(r, Err(CoreError::InvalidInput { .. }))
    }

    #[test]
    fn changes_need_a_link() {
        let dir = temp_dir("client-changes-unlinked");
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let client = Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let k = MessageKey {
            author: ALICE.into(),
            sent_at: 1,
        };
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            let not_linked = |r: Result<(), CoreError>| matches!(r, Err(CoreError::NotLinked));
            assert!(not_linked(client.react(k.clone(), "👍".into(), false).await));
            assert!(not_linked(client.edit(k.clone(), "new".into()).await));
            assert!(not_linked(client.delete_for_everyone(k).await));
            assert!(matches!(
                client.mark_read(conv.clone()).await,
                Err(CoreError::NotLinked)
            ));
        });
        assert!(matches!(client.set_typing(conv, true), Err(CoreError::NotLinked)));
    }

    #[test]
    fn edit_and_delete_only_own_recent_messages() {
        let (_dir, _sink, client, rt) = as_me("client-edit-rules");
        let now = client.next_sent_at();
        rt.block_on(async {
            for m in [
                stored(ME, now - 1000),
                stored(ALICE, now - 900),
                stored(ME, now - 25 * 3600 * 1000),
            ] {
                client.db.insert_message(&m).await.unwrap();
            }
            let key = |author: &str, sent_at| MessageKey {
                author: author.into(),
                sent_at,
            };
            // someone else's, too old, unknown, or no text
            assert!(invalid(client.edit(key(ALICE, now - 900), "new".into()).await));
            assert!(invalid(client.delete_for_everyone(key(ALICE, now - 900)).await));
            let old = key(ME, now - 25 * 3600 * 1000);
            assert!(invalid(client.edit(old.clone(), "new".into()).await));
            assert!(invalid(client.delete_for_everyone(old).await));
            assert!(invalid(client.edit(key(ME, now - 500), "new".into()).await));
            assert!(invalid(client.edit(key(ME, now - 1000), " ".into()).await));
            // own and recent: queued (shown once sent)
            client.edit(key(ME, now - 1000), "new".into()).await.unwrap();
            client.delete_for_everyone(key(ME, now - 1000)).await.unwrap();
            assert_eq!(
                client
                    .message(key(ME, now - 1000))
                    .await
                    .unwrap()
                    .unwrap()
                    .body
                    .as_deref(),
                Some("x")
            );
        });
    }

    #[test]
    fn reactions_need_an_emoji_and_a_message_that_is_still_there() {
        let (_dir, _sink, client, rt) = as_me("client-react");
        rt.block_on(async {
            let m = stored(ALICE, 10);
            client.db.insert_message(&m).await.unwrap();
            assert!(invalid(client.react(m.key(), "".into(), false).await));
            assert!(invalid(
                client
                    .react(
                        MessageKey {
                            author: ALICE.into(),
                            sent_at: 11
                        },
                        "👍".into(),
                        false
                    )
                    .await
            ));
            client.react(m.key(), "👍".into(), false).await.unwrap();
            client
                .db
                .apply(&crate::projection::Op::Delete { target: m.key() })
                .await
                .unwrap();
            assert!(invalid(client.react(m.key(), "👍".into(), false).await));
        });
    }

    #[test]
    fn mark_read_clears_unread_at_once() {
        let (_dir, sink, client, rt) = as_me("client-mark-read");
        let conv = format!("c:{ALICE}");
        rt.block_on(async {
            for m in [stored(ALICE, 1), stored(ME, 2), stored(ALICE, 3)] {
                client.db.insert_message(&m).await.unwrap();
            }
            assert_eq!(client.conversations().await.unwrap()[0].unread_count, 2);
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 2);
            // stored until sent, so a quit or a failed send can't lose them
            assert_eq!(
                client.db.unsent_receipts().await.unwrap(),
                vec![
                    MessageKey {
                        author: ALICE.into(),
                        sent_at: 1
                    },
                    MessageKey {
                        author: ALICE.into(),
                        sent_at: 3
                    }
                ]
            );
            assert_eq!(client.conversations().await.unwrap()[0].unread_count, 0);
            for t in [1, 3] {
                let key = MessageKey {
                    author: ALICE.into(),
                    sent_at: t,
                };
                assert_eq!(client.message(key).await.unwrap().unwrap().status, MessageStatus::Read);
            }
            assert_eq!(client.mark_read(conv.clone()).await.unwrap(), 0);
        });
        let changed = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, CoreEvent::MessageChanged { conversation_id, .. } if *conversation_id == conv))
            .count();
        assert_eq!(changed, 2);
    }

    #[test]
    fn typing_needs_a_conversation() {
        let (_dir, _sink, client, _rt) = as_me("client-typing");
        assert!(invalid(client.set_typing("c:nope".into(), true)));
        client.set_typing(format!("c:{ALICE}"), true).unwrap();
        client.set_typing(format!("c:{ALICE}"), false).unwrap();
    }
}
