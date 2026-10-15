use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::actor::{self, Command};
use crate::error::CoreError;
use crate::events::EventListener;
use crate::projection::{Conversation, Message, MessageKey, ProjectionDb, Reaction};

/// The app's handle on the core. Cheap to call from any thread; Signal work happens on the core
/// thread and reports back through the `EventListener`.
#[derive(uniffi::Object)]
pub struct Client {
    commands: mpsc::UnboundedSender<Command>,
    db: ProjectionDb,
    linked: Arc<AtomicBool>,
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
        let linked = Arc::new(AtomicBool::new(false));
        let h = actor::spawn(PathBuf::from(data_dir), db_key_hex, listener, linked.clone())?;
        Ok(Arc::new(Self {
            commands: h.commands,
            db: h.projection,
            linked,
        }))
    }

    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::SeqCst)
    }

    /// Starts linking as a secondary device: emits `LinkUrl`, then `Linked` or `Error`.
    pub fn link(&self, device_name: String) -> Result<(), CoreError> {
        self.commands
            .send(Command::Link { device_name })
            .map_err(|_| CoreError::Closed)
    }

    /// Starts receiving (idempotent). Emits `Connection`, `MessageChanged`, `Ready`, …
    pub fn start(&self) -> Result<(), CoreError> {
        self.commands.send(Command::Start).map_err(|_| CoreError::Closed)
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
        drop(Client::open(dir.0.to_string_lossy().into(), "ab".repeat(32), sink.clone()).unwrap());
        // the core thread may still be closing the files; retry briefly
        std::thread::sleep(Duration::from_millis(200));
        assert!(Client::open(dir.0.to_string_lossy().into(), "cd".repeat(32), sink).is_err());
    }
}
