//! The core thread. presage's `Manager` and store are `!Send`, so everything that touches them runs
//! here: one OS thread with a current-thread tokio runtime and a `LocalSet`. The rest of the core
//! talks to it through `Command`s; results flow back as `CoreEvent`s.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use futures::{channel::oneshot as fut_oneshot, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::protocol::Aci;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::ContentsStore;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::CoreError;
use crate::events::{alert, from_applied, ConnectionState, CoreEvent, EventListener};
use crate::projection::ids::{direct_id, group_id};
use crate::projection::{project, Op, ProjectionDb};
use crate::send::{sender, Job, Jobs};
use crate::typing::{Typing, TYPING_IDLE};

/// libsignal's post-quantum crypto (libcrux ML-KEM key generation, also run by presage's pre-key
/// refresh while receiving, and session decryption) uses very large stack frames in debug builds;
/// the default 2 MB thread stack overflowed both while linking and while receiving. This is the fix.
const CORE_THREAD_STACK: usize = 64 * 1024 * 1024;

/// The builder for the core thread (shared with tests so they exercise the same stack size).
pub(crate) fn core_thread() -> std::thread::Builder {
    std::thread::Builder::new()
        .name("sig-core".into())
        .stack_size(CORE_THREAD_STACK)
}

/// How long a linking QR code stays valid before the app has to ask for a new one.
const LINK_TIMEOUT: Duration = Duration::from_secs(300);
/// A connection must stay up this long before the reconnect backoff resets.
const STABLE_CONNECTION: Duration = Duration::from_secs(30);

pub enum Command {
    Link {
        device_name: String,
    },
    Start,
    Send(Job),
    /// The user is typing in a conversation (`started`), or stopped.
    Typing {
        conversation_id: String,
        started: bool,
    },
}

pub struct Handles {
    pub commands: mpsc::UnboundedSender<Command>,
    pub projection: ProjectionDb,
    pub thread: JoinHandle<()>,
}

/// Starts the core thread, opens both databases and waits until they are ready. `linked` is kept
/// up to date by the thread. Fails if another core has `data_dir` open.
pub fn spawn(
    data_dir: PathBuf,
    key: String,
    listener: Arc<dyn EventListener>,
    linked: Arc<AtomicBool>,
    own_aci: Arc<Mutex<Option<String>>>,
) -> Result<Handles, CoreError> {
    std::fs::create_dir_all(&data_dir).map_err(|e| CoreError::Storage { message: e.to_string() })?;
    // Two cores on one presage store would fight over the websocket and corrupt sessions.
    let lock = File::create(data_dir.join(".lock")).map_err(|e| CoreError::Storage { message: e.to_string() })?;
    lock.try_lock().map_err(|_| CoreError::AlreadyOpen)?;
    let (commands, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let thread = core_thread()
        .spawn(move || {
            let _lock = lock; // held for the thread's lifetime
            let _guard = PanicReporter(listener.clone());
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(CoreError::Storage { message: e.to_string() }));
                    return;
                }
            };
            tokio::task::LocalSet::new().block_on(
                &rt,
                Box::pin(run(data_dir, key, listener, linked, own_aci, rx, ready_tx)),
            );
        })
        .map_err(|e| CoreError::Storage { message: e.to_string() })?;
    match ready_rx.blocking_recv() {
        Ok(Ok(projection)) => Ok(Handles {
            commands,
            projection,
            thread,
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(CoreError::Closed)
        }
    }
}

/// Tells the app if the core thread dies from a panic, instead of going silent.
struct PanicReporter(Arc<dyn EventListener>);

impl Drop for PanicReporter {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.on_event(CoreEvent::Error {
                message: "the core stopped unexpectedly; restart Semaphore".into(),
            });
            self.0.on_event(CoreEvent::Connection {
                state: ConnectionState::Offline,
            });
        }
    }
}

/// SQLCipher options with a raw 256-bit key (no key derivation) and statement logging off, so the
/// key pragma never reaches a log.
pub(crate) fn sqlite_options(path: &std::path::Path, key: &str) -> Result<SqliteConnectOptions, CoreError> {
    Ok(SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .foreign_keys(true)
        .disable_statement_logging()
        .pragma("key", format!("\"x'{key}'\"")))
}

async fn run(
    dir: PathBuf,
    key: String,
    listener: Arc<dyn EventListener>,
    linked: Arc<AtomicBool>,
    own_aci: Arc<Mutex<Option<String>>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    ready: oneshot::Sender<Result<ProjectionDb, CoreError>>,
) {
    let opened = async {
        // ProjectionDb::open validates the key format before anything is interpolated.
        let projection =
            ProjectionDb::open(&format!("sqlite://{}", dir.join("projection.db").display()), Some(&key)).await?;
        let store =
            SqliteStore::open_with_options(sqlite_options(&dir.join("presage.db"), &key)?, OnNewIdentity::Trust)
                .await
                .map_err(|e| CoreError::Storage { message: e.to_string() })?;
        // Only "not registered" means unlinked. Any other error must fail loudly: linking starts by
        // clearing the registration, so mistaking a read error for "unlinked" would destroy it.
        let manager = match Manager::load_registered(store.clone()).await {
            Ok(m) => Some(m),
            Err(presage::Error::NotYetRegisteredError) => None,
            Err(e) => {
                return Err(CoreError::Storage {
                    message: format!("loading the registration failed: {e}"),
                })
            }
        };
        Ok::<_, CoreError>((store, projection, manager))
    };
    let (store, projection, mut manager) = match opened.await {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    linked.store(manager.is_some(), Ordering::SeqCst);
    let set_aci = |m: &Manager<SqliteStore, Registered>| {
        if let Ok(mut aci) = own_aci.lock() {
            *aci = Some(m.registration_data().service_ids.aci.to_string());
        }
    };
    if let Some(m) = &manager {
        set_aci(m);
    }
    // Sends queue here until receiving starts and the server queue is drained.
    let (send_tx, send_rx) = mpsc::unbounded_channel::<Job>();
    let jobs: Jobs = Rc::new(RefCell::new(send_rx));
    let typing = Rc::new(RefCell::new(Typing::default()));
    let mut sending = false;
    // Sends queued by a previous run died with it.
    match projection.fail_all_pending().await {
        Ok(failed) => {
            for (conversation_id, key) in failed {
                listener.on_event(CoreEvent::MessageChanged { conversation_id, key });
            }
        }
        Err(e) => listener.on_event(CoreEvent::Error {
            message: format!("checking unsent messages failed: {e}"),
        }),
    }
    let (caught_up_tx, caught_up_rx) = watch::channel(false);
    let caught_up_tx = Rc::new(caught_up_tx);
    if ready.send(Ok(projection.clone())).is_err() {
        return;
    }

    let receiving = Rc::new(Cell::new(false));
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Link { device_name } => {
                if manager.is_some() {
                    listener.on_event(CoreEvent::Error {
                        message: "already linked".into(),
                    });
                    continue;
                }
                // boxed to keep the large provisioning future off the stack (the stack size is the real fix)
                match Box::pin(link(&store, device_name, listener.as_ref())).await {
                    Ok(m) => {
                        set_aci(&m);
                        manager = Some(m);
                        linked.store(true, Ordering::SeqCst);
                        listener.on_event(CoreEvent::Linked);
                    }
                    Err(message) => {
                        listener.on_event(CoreEvent::Error { message });
                        listener.on_event(CoreEvent::Connection {
                            state: ConnectionState::Unlinked,
                        });
                    }
                }
            }
            Command::Start => match &manager {
                Some(_) if receiving.get() => {}
                Some(m) => {
                    receiving.set(true);
                    if !sending {
                        sending = true;
                        tokio::task::spawn_local(supervise_sender(
                            m.clone(),
                            projection.clone(),
                            listener.clone(),
                            jobs.clone(),
                            caught_up_rx.clone(),
                        ));
                    }
                    let task = tokio::task::spawn_local(Box::pin(receive_loop(
                        m.clone(),
                        projection.clone(),
                        listener.clone(),
                        caught_up_tx.clone(),
                        send_tx.clone(),
                    )));
                    let (receiving, l) = (receiving.clone(), listener.clone());
                    // Supervise: a panic in the loop must be reported and allow a restart.
                    tokio::task::spawn_local(async move {
                        if let Err(e) = task.await {
                            l.on_event(CoreEvent::Error {
                                message: format!("receiving stopped: {e}"),
                            });
                            l.on_event(CoreEvent::Connection {
                                state: ConnectionState::Offline,
                            });
                        }
                        receiving.set(false);
                    });
                }
                None => listener.on_event(CoreEvent::Connection {
                    state: ConnectionState::Unlinked,
                }),
            },
            Command::Send(job) => {
                if let Err(mpsc::error::SendError(job)) = send_tx.send(job) {
                    listener.on_event(CoreEvent::Error {
                        message: "the sender stopped".into(),
                    });
                    if let Job::Text(job) = job {
                        let _ = projection.set_failed(&job.key).await;
                        listener.on_event(CoreEvent::MessageChanged {
                            conversation_id: job.conversation_id,
                            key: job.key,
                        });
                    }
                }
            }
            Command::Typing {
                conversation_id,
                started,
            } => {
                let now = std::time::Instant::now();
                let send = if started {
                    typing.borrow_mut().started(&conversation_id, now)
                } else {
                    typing.borrow_mut().stopped(&conversation_id)
                };
                if send {
                    let _ = send_tx.send(Job::Typing {
                        conversation_id,
                        started,
                        queued: now,
                    });
                }
                if started {
                    // typing stops on its own once no keystroke came for TYPING_IDLE
                    let (typing, send_tx) = (typing.clone(), send_tx.clone());
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(TYPING_IDLE).await;
                        let now = std::time::Instant::now();
                        for conversation_id in typing.borrow_mut().expire(now) {
                            let _ = send_tx.send(Job::Typing {
                                conversation_id,
                                started: false,
                                queued: now,
                            });
                        }
                    });
                }
            }
        }
    }
}

/// Links as a secondary device: emits `LinkUrl`, waits (bounded) for the phone to scan, then asks
/// the phone for contacts. Returns a message for the user on failure.
async fn link(
    store: &SqliteStore,
    device_name: String,
    l: &dyn EventListener,
) -> Result<Manager<SqliteStore, Registered>, String> {
    let (url_tx, url_rx) = fut_oneshot::channel();
    let linking = futures::future::join(
        Manager::link_secondary_device(store.clone(), SignalServers::Production, device_name, url_tx),
        async move {
            if let Ok(url) = url_rx.await {
                l.on_event(CoreEvent::LinkUrl { url: url.to_string() });
            }
        },
    );
    let (result, _) = tokio::time::timeout(LINK_TIMEOUT, linking)
        .await
        .map_err(|_| "the code expired before it was scanned".to_string())?;
    let mut m = result.map_err(|e| format!("linking failed: {e}"))?;
    if let Err(e) = m.request_contacts().await {
        l.on_event(CoreEvent::Error {
            message: format!("couldn't ask the phone for contacts: {e}"),
        });
    }
    Ok(m)
}

/// Runs the sender and restarts it if it panics. Jobs queued meanwhile stay queued; the one in
/// flight is lost, so every Pending row is marked Failed (retryable). Unsent read receipts are
/// stored and go out with the next flush; a lost reaction, edit or delete is not retried.
async fn supervise_sender(
    m: Manager<SqliteStore, Registered>,
    db: ProjectionDb,
    l: Arc<dyn EventListener>,
    jobs: Jobs,
    caught_up: watch::Receiver<bool>,
) {
    loop {
        let task = tokio::task::spawn_local(Box::pin(sender(
            m.clone(),
            db.clone(),
            l.clone(),
            jobs.clone(),
            caught_up.clone(),
        )));
        match task.await {
            Ok(()) => return,
            Err(e) => {
                l.on_event(CoreEvent::Error {
                    message: format!("sending stopped unexpectedly: {e}"),
                });
                if let Ok(failed) = db.fail_all_pending().await {
                    for (conversation_id, key) in failed {
                        l.on_event(CoreEvent::MessageChanged { conversation_id, key });
                    }
                }
            }
        }
    }
}

/// Receives forever, reconnecting with exponential backoff (1 s → 60 s).
async fn receive_loop(
    mut m: Manager<SqliteStore, Registered>,
    db: ProjectionDb,
    l: Arc<dyn EventListener>,
    caught_up: Rc<watch::Sender<bool>>,
    jobs: mpsc::UnboundedSender<Job>,
) {
    let me = m.registration_data().service_ids.aci.to_string();
    let mut backoff = Duration::from_secs(1);
    let mut progress = Progress::default();
    loop {
        l.on_event(CoreEvent::Connection {
            state: ConnectionState::Connecting,
        });
        match Box::pin(m.receive_messages()).await {
            Ok(stream) => {
                let connected = tokio::time::Instant::now();
                l.on_event(CoreEvent::Connection {
                    state: ConnectionState::Online,
                });
                futures::pin_mut!(stream);
                // what the server queued while we were away is history, not news
                progress.live = false;
                while let Some(received) = stream.next().await {
                    handle(received, &m, &db, &me, &mut progress, l.as_ref(), &jobs).await;
                    caught_up.send_if_modified(|ready| {
                        let first = !*ready && progress.titles_synced;
                        *ready |= progress.titles_synced;
                        first
                    });
                }
                if connected.elapsed() >= STABLE_CONNECTION {
                    backoff = Duration::from_secs(1);
                }
                l.on_event(CoreEvent::Error {
                    message: "the connection to the server closed".into(),
                });
            }
            Err(e) => l.on_event(CoreEvent::Error {
                message: format!("connecting failed: {e}"),
            }),
        }
        l.on_event(CoreEvent::Connection {
            state: ConnectionState::Offline,
        });
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// How far receiving has got.
#[derive(Default)]
struct Progress {
    /// Contact and group names were copied once (after the first drained queue).
    titles_synced: bool,
    /// This connection has drained the server queue: what arrives now is new.
    live: bool,
}

async fn handle(
    received: Received,
    m: &Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    me: &str,
    progress: &mut Progress,
    l: &dyn EventListener,
    jobs: &mpsc::UnboundedSender<Job>,
) {
    // Kinds and counts only: never message content.
    match &received {
        Received::QueueEmpty => tracing::info!("caught up with the server queue"),
        Received::Contacts => tracing::info!("contacts synced from the phone"),
        Received::DecryptionError(_) => {}
        Received::Content(c) => tracing::info!(kind = content_kind(&c.body), "received"),
    }
    match received {
        Received::QueueEmpty => {
            progress.live = true;
            if !progress.titles_synced {
                sync_titles(m, db, l).await;
                progress.titles_synced = true;
            }
            l.on_event(CoreEvent::Ready);
        }
        Received::Contacts => sync_titles(m, db, l).await,
        // Ids only: never log message content.
        Received::DecryptionError(sender) => l.on_event(CoreEvent::Error {
            message: format!("a message from {} couldn't be decrypted", sender.service_id_string()),
        }),
        Received::Content(content) => {
            let ops = project(&content, me);
            tracing::info!(ops = ops.len(), "projected");
            let sender = content.metadata.sender.service_id_string();
            if apply_received(db, l, &ops, &sender, me, progress.live).await {
                // receipts held back while the read receipts setting was unknown; queued only
                // now that the new setting is stored, so they can't go out under the old one
                let _ = jobs.send(Job::Receipts);
            }
        }
    }
}

/// Applies the ops of one received content and, when `live` (not catching up), announces what
/// is worth a notification. True if it changed the privacy settings.
async fn apply_received(
    db: &ProjectionDb,
    l: &dyn EventListener,
    ops: &[Op],
    sender: &str,
    me: &str,
    live: bool,
) -> bool {
    let mut settings_changed = false;
    for op in ops {
        match db.apply(op).await {
            Ok(applied) => {
                settings_changed |= matches!(op, Op::Settings { .. });
                let new = if live { alert(op, &applied, me) } else { None };
                applied.into_iter().for_each(|a| l.on_event(from_applied(a)));
                if let Some(e) = new {
                    l.on_event(e);
                }
            }
            Err(e) => l.on_event(CoreEvent::Error {
                message: format!("storing a message from {sender} failed: {e}"),
            }),
        }
    }
    settings_changed
}

fn content_kind(body: &presage::libsignal_service::content::ContentBody) -> &'static str {
    use presage::libsignal_service::content::ContentBody::*;
    match body {
        NullMessage(_) => "null",
        DataMessage(_) => "data",
        SynchronizeMessage(_) => "sync",
        CallMessage(_) => "call",
        ReceiptMessage(_) => "receipt",
        TypingMessage(_) => "typing",
        DecryptionErrorMessage(_) => "decryption-error",
        StoryMessage(_) => "story",
        EditMessage(_) => "edit",
    }
}

/// Copies contact names and group titles from presage's store into the projection.
async fn sync_titles(m: &Manager<SqliteStore, Registered>, db: &ProjectionDb, l: &dyn EventListener) {
    let mut problems: Vec<String> = vec![];
    match m.store().contacts().await {
        Ok(contacts) => {
            for contact in contacts {
                match contact {
                    Ok(c) if c.name.is_empty() => {}
                    Ok(c) => {
                        let id = direct_id(&Aci::from(c.uuid).into());
                        if let Err(e) = db.upsert_conversation_title(&id, &c.name).await {
                            problems.push(format!("storing a contact name: {e}"));
                        }
                    }
                    Err(e) => problems.push(format!("reading a contact: {e}")),
                }
            }
        }
        Err(e) => problems.push(format!("reading contacts: {e}")),
    }
    match m.store().groups().await {
        Ok(groups) => {
            for group in groups {
                match group {
                    Ok((key, g)) => {
                        if let Err(e) = db.upsert_conversation_title(&group_id(&key), &g.title).await {
                            problems.push(format!("storing a group title: {e}"));
                        }
                    }
                    Err(e) => problems.push(format!("reading a group: {e}")),
                }
            }
        }
        Err(e) => problems.push(format!("reading groups: {e}")),
    }
    if let Some(first) = problems.first() {
        l.on_event(CoreEvent::Error {
            message: format!("{} names couldn't be synced (first: {first})", problems.len()),
        });
    }
    l.on_event(CoreEvent::ConversationsChanged);
}

#[cfg(test)]
mod tests {
    use presage::libsignal_service::protocol::kem::{KeyPair, KeyType};

    use crate::events::{CoreEvent, EventListener};
    use crate::projection::{Op, ProjectionDb, Settings};

    struct Quiet;
    impl EventListener for Quiet {
        fn on_event(&self, _: CoreEvent) {}
    }

    struct Sink(std::sync::Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    /// Catching up after being offline must not set off a notification per queued message.
    #[tokio::test]
    async fn only_live_messages_are_announced() {
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let msg = |sent_at| {
            Op::InsertMessage(crate::projection::Message {
                conversation_id: "c:alice".into(),
                author: "alice".into(),
                sent_at,
                received_at: sent_at,
                body: Some("hi".into()),
                is_outgoing: false,
                status: crate::projection::MessageStatus::Delivered,
                edited: false,
                deleted: false,
                quote: None,
            })
        };
        let sink = Sink(Default::default());
        let announced = |sink: &Sink| {
            sink.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, CoreEvent::NewMessage { .. }))
                .count()
        };
        super::apply_received(&db, &sink, &[msg(1)], "alice", "me", false).await;
        assert_eq!(announced(&sink), 0);
        super::apply_received(&db, &sink, &[msg(2)], "alice", "me", true).await;
        assert_eq!(announced(&sink), 1);
    }

    /// The receipts flush is queued after this returns, so the new setting must already be stored.
    #[tokio::test]
    async fn a_settings_change_is_stored_before_it_is_reported() {
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let off = Op::Settings {
            read_receipts: Some(false),
            typing_indicators: None,
        };
        assert!(super::apply_received(&db, &Quiet, &[off], "me", "me", true).await);
        assert_eq!(db.settings().await.unwrap().read_receipts, Some(false));
        let typing = Op::Typing {
            conversation_id: "c:a".into(),
            who: "a".into(),
            started: true,
        };
        assert!(!super::apply_received(&db, &Quiet, &[typing], "a", "me", true).await);
        assert_eq!(
            db.settings().await.unwrap(),
            Settings {
                read_receipts: Some(false),
                typing_indicators: None
            }
        );
    }

    /// Linking generates Kyber keys on the core thread (on top of presage's linking future); the
    /// default 2 MB stack overflowed there in debug builds.
    #[test]
    fn core_thread_has_room_for_kyber_key_generation() {
        let generated = super::core_thread()
            .spawn(|| {
                let mut rng = rand::rng();
                KeyPair::generate(KeyType::Kyber1024, &mut rng)
                    .public_key
                    .serialize()
                    .len()
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(generated > 1000);
    }
}
