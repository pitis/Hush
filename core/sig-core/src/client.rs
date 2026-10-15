use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::actor::{self, Command};
use crate::error::CoreError;
use crate::events::{CoreEvent, EventListener};
use crate::projection::{Conversation, Message, MessageKey, MessageStatus, ProjectionDb, Quote, Reaction};
use crate::send::SendJob;

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
        let queued = self.send(Command::Send(SendJob {
            key: key.clone(),
            conversation_id,
            body,
            quote,
        }));
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
        self.send(Command::Send(SendJob {
            key,
            conversation_id: m.conversation_id,
            body,
            quote: m.quote,
        }))?;
        Ok(true)
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
}
