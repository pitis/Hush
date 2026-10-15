use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::actor::{self, Command};
use crate::error::CoreError;
use crate::events::EventListener;
use crate::projection::{Conversation, Message, MessageKey, ProjectionDb, Reaction};

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
        let h = actor::spawn(PathBuf::from(data_dir), db_key_hex, listener, linked.clone())?;
        Ok(Arc::new(Self {
            commands: Mutex::new(Some(h.commands)),
            thread: Mutex::new(Some(h.thread)),
            db: h.projection,
            linked,
        }))
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
}
