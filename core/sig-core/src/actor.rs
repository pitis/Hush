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
use presage::libsignal_service::content::Content;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::{ContentsStore, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use tokio::sync::{mpsc, oneshot, watch};

use crate::download::{downloader, Download, Downloads};
use crate::error::CoreError;
use crate::events::{alert, from_applied, ConnectionState, CoreEvent, EventListener};
use crate::projection::ids::{direct_id, group_id};
use crate::projection::{project, ChatEventKind, Op, ProjectionDb};
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
    /// Download one attachment (the user asked for it).
    Download(i64),
    /// The user is typing in a conversation (`started`), or stopped.
    Typing {
        conversation_id: String,
        started: bool,
    },
    /// Reconnect now (the network came back, or the user asked): skips the wait, and drops a
    /// connection that may have died with the old network.
    Reconnect,
    /// Starts a new secure session with a contact whose messages couldn't be decrypted.
    ResetSession {
        conversation_id: String,
    },
    /// Stops the core for good, after telling the server this device is unlinked if
    /// `tell_server` (an error if that fails: nothing is stopped then). The caller removes the
    /// data once the thread has ended.
    Unlink {
        tell_server: bool,
        done: oneshot::Sender<Result<(), String>>,
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
        let projection = ProjectionDb::open(&format!("sqlite://{}", dir.join("projection.db").display()), Some(&key))
            .await?
            .with_files(dir.join("attachments"));
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
    // Downloads queue here until receiving starts.
    let (download_tx, download_rx) = mpsc::unbounded_channel::<Download>();
    let downloads: Downloads = Rc::new(RefCell::new(download_rx));
    let mut downloading = false;
    let queues = Queues {
        sends: send_tx.clone(),
        downloads: download_tx.clone(),
    };
    // Downloads cut off by the previous run start over.
    if let Err(e) = projection.reset_downloads().await {
        listener.on_event(CoreEvent::Error {
            message: format!("checking unfinished downloads failed: {e}"),
        });
    }
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

    // Disappearing messages go once their timer runs out, connected or not.
    tokio::task::spawn_local(expire(store.clone(), projection.clone(), listener.clone()));
    let receiving = Rc::new(Cell::new(false));
    let wake = Rc::new(tokio::sync::Notify::new());
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
                    if !downloading {
                        downloading = true;
                        tokio::task::spawn_local(supervise_downloader(
                            m.clone(),
                            projection.clone(),
                            listener.clone(),
                            downloads.clone(),
                        ));
                        let _ = download_tx.send(Download::Auto);
                    }
                    let task = tokio::task::spawn_local(Box::pin(receive_loop(
                        m.clone(),
                        projection.clone(),
                        listener.clone(),
                        caught_up_tx.clone(),
                        queues.clone(),
                        wake.clone(),
                        linked.clone(),
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
            // wakes only what waits now: a stored request would later drop a fresh connection
            Command::Reconnect => wake.notify_waiters(),
            // presage resets sessions by ACI only (it panics on a PNI)
            Command::ResetSession { conversation_id } => match (&manager, crate::send::recipient(&conversation_id)) {
                (Some(m), Some(crate::send::Recipient::Direct(to @ ServiceId::Aci(_)))) => {
                    let (mut m, l, db) = (m.clone(), listener.clone(), projection.clone());
                    tokio::task::spawn_local(async move {
                        let at = chrono::Utc::now().timestamp_millis();
                        if let Err(e) = Box::pin(m.send_session_reset(&to, at as u64)).await {
                            return l.on_event(CoreEvent::Error {
                                message: format!("resetting the session with {conversation_id} failed: {e}"),
                            });
                        }
                        let author = to.raw_uuid().to_string();
                        match db
                            .add_chat_event(&conversation_id, at, ChatEventKind::SessionReset, &author)
                            .await
                        {
                            Ok(()) => l.on_event(CoreEvent::ChatEventAdded { conversation_id }),
                            Err(e) => l.on_event(CoreEvent::Error {
                                message: format!("recording a session reset failed: {e}"),
                            }),
                        }
                    });
                }
                _ => listener.on_event(CoreEvent::Error {
                    message: format!("a session can't be reset with {conversation_id}"),
                }),
            },
            Command::Unlink { tell_server, done } => {
                if let (true, Some(m)) = (tell_server, &manager) {
                    match tokio::time::timeout(Duration::from_secs(15), Box::pin(m.unlink_self())).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            let _ = done.send(Err(e.to_string()));
                            continue;
                        }
                        Err(_) => {
                            let _ = done.send(Err("the server didn't answer".into()));
                            continue;
                        }
                    }
                }
                linked.store(false, Ordering::SeqCst);
                let _ = done.send(Ok(()));
                // everything on this thread (receiving, sending, downloads) ends with it
                return;
            }
            Command::Download(id) => {
                let _ = download_tx.send(Download::One(id));
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
                l.on_event(CoreEvent::LinkUrl {
                    url: crate::history::with_backup_capability(url.as_str()),
                });
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

/// How often messages whose disappearing timer ran out are looked for.
const EXPIRY_SWEEP: Duration = Duration::from_secs(5);

/// Removes disappearing messages as their timers run out, for as long as the core runs.
async fn expire(mut store: SqliteStore, db: ProjectionDb, l: Arc<dyn EventListener>) {
    // an error is reported once until a sweep works again, not every 5 s
    let mut failing = false;
    loop {
        let swept = sweep(&mut store, &db, l.as_ref()).await;
        match swept {
            Ok(()) => failing = false,
            Err(e) if !failing => {
                failing = true;
                l.on_event(CoreEvent::Error {
                    message: format!("removing disappearing messages failed: {e}"),
                });
            }
            Err(_) => {}
        }
        tokio::time::sleep(EXPIRY_SWEEP).await;
    }
}

/// One expiry sweep: removes expired messages, then their copies in presage's store (queued
/// durably, so one that fails, or is cut short by a quit, is retried by the next sweep).
async fn sweep(store: &mut SqliteStore, db: &ProjectionDb, l: &dyn EventListener) -> Result<(), String> {
    let gone = db
        .delete_expired(chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|e| e.to_string())?;
    if !gone.is_empty() {
        tracing::info!(count = gone.len(), "disappearing messages removed");
    }
    for e in &gone {
        l.on_event(CoreEvent::MessageExpired {
            conversation_id: e.conversation_id.clone(),
            key: e.key.clone(),
        });
    }
    if !gone.is_empty() {
        l.on_event(CoreEvent::ConversationsChanged);
    }
    let mut first_error = None;
    for (conversation_id, sent_at) in db.presage_forget_queue().await.map_err(|e| e.to_string())? {
        let done = match forget_in_presage(store, &conversation_id, sent_at).await {
            Ok(()) => db
                .presage_forgotten(&conversation_id, sent_at)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        if let Err(e) = done {
            first_error.get_or_insert(format!("deleting a stored copy: {e}"));
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Removes one message (or edit) from presage's store, which keeps its own copy of each.
async fn forget_in_presage(store: &mut SqliteStore, conversation_id: &str, sent_at: i64) -> Result<(), String> {
    let thread = crate::send::thread(conversation_id).ok_or("not a conversation id")?;
    store
        .delete_message(&thread, sent_at as u64)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Keeps a chat's timer as presage has it: presage orders timer changes by version (1:1) or
/// group state, which the projection doesn't see.
async fn sync_timer(store: &SqliteStore, db: &ProjectionDb, l: &dyn EventListener, content: &Content) {
    let Ok(thread) = Thread::try_from(content) else { return };
    let conversation_id = match &thread {
        Thread::Contact(sid) => direct_id(sid),
        Thread::Group(key) => group_id(key),
    };
    let timer = match store.expire_timer(&thread).await {
        Ok(t) => t.map_or(0, |(t, _)| t),
        Err(e) => {
            l.on_event(CoreEvent::Error {
                message: format!("reading a chat's timer failed: {e}"),
            });
            return;
        }
    };
    match db.set_chat_timer(&conversation_id, timer).await {
        Ok(true) => l.on_event(CoreEvent::ConversationsChanged),
        Ok(false) => {}
        Err(e) => l.on_event(CoreEvent::Error {
            message: format!("storing a chat's timer failed: {e}"),
        }),
    }
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

/// Runs the downloader and restarts it if it panics: queued downloads stay queued, and the one in
/// flight starts over.
async fn supervise_downloader(
    m: Manager<SqliteStore, Registered>,
    db: ProjectionDb,
    l: Arc<dyn EventListener>,
    downloads: Downloads,
) {
    loop {
        let task = tokio::task::spawn_local(Box::pin(downloader(
            m.clone(),
            db.clone(),
            l.clone(),
            downloads.clone(),
        )));
        match task.await {
            Ok(()) => return,
            Err(e) => {
                l.on_event(CoreEvent::Error {
                    message: format!("downloading stopped unexpectedly: {e}"),
                });
                if let Err(e) = db.reset_downloads().await {
                    l.on_event(CoreEvent::Error {
                        message: format!("checking unfinished downloads failed: {e}"),
                    });
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
    queues: Queues,
    wake: Rc<tokio::sync::Notify>,
    linked: Arc<AtomicBool>,
) {
    let me = m.registration_data().service_ids.aci.to_string();
    let mut backoff = Duration::from_secs(1);
    let mut progress = Progress::default();
    loop {
        l.on_event(CoreEvent::Connection {
            state: ConnectionState::Connecting,
        });
        // a reconnect request also cuts short a connect that hangs (Wi-Fi without internet):
        // nothing has been read yet, so nothing is lost
        let connecting = tokio::select! {
            connected = Box::pin(m.receive_messages()) => Some(connected),
            _ = wake.notified() => None,
        };
        let Some(connecting) = connecting else {
            backoff = Duration::from_secs(1);
            continue;
        };
        match connecting {
            Ok(stream) => {
                let connected = tokio::time::Instant::now();
                l.on_event(CoreEvent::Connection {
                    state: ConnectionState::Online,
                });
                futures::pin_mut!(stream);
                progress.connected();
                let mut woken = false;
                loop {
                    let received = tokio::select! {
                        received = stream.next() => received,
                        // asked to reconnect: this connection may be dead with the old network
                        _ = wake.notified() => {
                            woken = true;
                            None
                        }
                    };
                    let Some(received) = received else { break };
                    handle(received, &m, &db, &me, &mut progress, l.as_ref(), &queues).await;
                    caught_up.send_if_modified(|ready| {
                        let first = !*ready && progress.titles_synced;
                        *ready |= progress.titles_synced;
                        first
                    });
                }
                if woken {
                    // The server already counts what this connection read as delivered: handle
                    // all of it before dropping the connection (it ends once nothing comes for
                    // a moment; a dead one has nothing more).
                    while let Ok(Some(received)) = tokio::time::timeout(DRAIN, stream.next()).await {
                        handle(received, &m, &db, &me, &mut progress, l.as_ref(), &queues).await;
                    }
                    backoff = Duration::from_secs(1);
                    continue;
                }
                if connected.elapsed() >= STABLE_CONNECTION {
                    backoff = Duration::from_secs(1);
                }
                l.on_event(CoreEvent::Error {
                    message: "the connection to the server closed".into(),
                });
            }
            Err(presage::Error::ServiceError(e)) if is_unlinked(&e) => {
                // removed on the phone: retrying can't help, the app offers to link again
                tracing::warn!("this device is no longer linked");
                linked.store(false, Ordering::SeqCst);
                l.on_event(CoreEvent::Connection {
                    state: ConnectionState::Unlinked,
                });
                return;
            }
            Err(e) => l.on_event(CoreEvent::Error {
                message: format!("connecting failed: {e}"),
            }),
        }
        l.on_event(CoreEvent::Connection {
            state: ConnectionState::Offline,
        });
        if pause(backoff, &wake).await {
            backoff = Duration::from_secs(1);
        } else {
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}

/// On a reconnect request, the old connection is read until it is quiet this long.
const DRAIN: Duration = Duration::from_secs(1);

/// Waits `backoff` before reconnecting, or less if woken. True if woken.
async fn pause(backoff: Duration, wake: &tokio::sync::Notify) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => false,
        _ = wake.notified() => true,
    }
}

/// The server saying this device isn't linked any more: a 401/403, either as an answer or when
/// the websocket is set up (the phone removed it, or the account is gone).
fn is_unlinked(e: &presage::libsignal_service::prelude::ServiceError) -> bool {
    use presage::libsignal_service::prelude::ServiceError;
    match e {
        ServiceError::Unauthorized => true,
        ServiceError::WsError(ws) => matches!(
            ws.as_ref(),
            reqwest_websocket::Error::Handshake(reqwest_websocket::HandshakeError::UnexpectedStatusCode(status))
                if *status == http::StatusCode::UNAUTHORIZED || *status == http::StatusCode::FORBIDDEN
        ),
        _ => false,
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

impl Progress {
    /// A new connection: what the server queued while we were away is history, not news.
    fn connected(&mut self) {
        self.live = false;
    }

    /// The server queue is drained. True the first time ever: names need syncing then.
    fn drained(&mut self) -> bool {
        self.live = true;
        !std::mem::replace(&mut self.titles_synced, true)
    }
}

async fn handle(
    received: Received,
    m: &Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    me: &str,
    progress: &mut Progress,
    l: &dyn EventListener,
    queues: &Queues,
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
            if progress.drained() {
                sync_titles(m, db, l).await;
            }
            l.on_event(CoreEvent::Ready);
        }
        Received::Contacts => sync_titles(m, db, l).await,
        // Ids only: never log message content.
        Received::DecryptionError(sender) => {
            l.on_event(CoreEvent::Error {
                message: format!("a message from {} couldn't be decrypted", sender.service_id_string()),
            });
            // shown in the chat, with a way to reset the session
            let conversation_id = direct_id(&sender);
            let at = chrono::Utc::now().timestamp_millis();
            let author = sender.raw_uuid().to_string();
            match db
                .add_chat_event(&conversation_id, at, ChatEventKind::DecryptionFailed, &author)
                .await
            {
                Ok(()) => l.on_event(CoreEvent::ChatEventAdded { conversation_id }),
                Err(e) => l.on_event(CoreEvent::Error {
                    message: format!("recording an undecryptable message failed: {e}"),
                }),
            }
        }
        Received::Content(content) => {
            let ops = project(&content, me);
            tracing::info!(ops = ops.len(), "projected");
            let sender = content.metadata.sender.service_id_string();
            let outcome = apply_received(db, l, &ops, &sender, me, progress.live).await;
            sync_timer(m.store(), db, l, &content).await;
            if outcome.settings_changed {
                // receipts held back while the read receipts setting was unknown; queued only
                // now that the new setting is stored, so they can't go out under the old one
                let _ = queues.sends.send(Job::Receipts);
            }
            if outcome.attachments_added {
                let _ = queues.downloads.send(Download::Auto);
            }
        }
    }
}

/// Where the receive loop hands work on to.
#[derive(Clone)]
struct Queues {
    sends: mpsc::UnboundedSender<Job>,
    downloads: mpsc::UnboundedSender<Download>,
}

/// What applying one received content changed beyond the projection.
#[derive(Default)]
struct Outcome {
    settings_changed: bool,
    attachments_added: bool,
}

/// Applies the ops of one received content and, when `live` (not catching up), announces what
/// is worth a notification.
async fn apply_received(
    db: &ProjectionDb,
    l: &dyn EventListener,
    ops: &[Op],
    sender: &str,
    me: &str,
    live: bool,
) -> Outcome {
    let mut outcome = Outcome::default();
    for op in ops {
        match db.apply(op).await {
            Ok(applied) => {
                outcome.settings_changed |= matches!(op, Op::Settings { .. });
                outcome.attachments_added |= matches!(op, Op::AddAttachments { .. }) && !applied.is_empty();
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
    outcome
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
                    Ok(c) => {
                        let id = direct_id(&Aci::from(c.uuid).into());
                        if !c.name.is_empty() {
                            if let Err(e) = db.upsert_conversation_title(&id, &c.name).await {
                                problems.push(format!("storing a contact name: {e}"));
                            }
                        }
                        // chats that disappeared before we knew of timers, or changed while away
                        if let Err(e) = db.set_chat_timer(&id, c.expire_timer).await {
                            problems.push(format!("storing a chat's timer: {e}"));
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
                        let timer = g.disappearing_messages_timer.map_or(0, |t| t.duration);
                        if let Err(e) = db.set_chat_timer(&group_id(&key), timer).await {
                            problems.push(format!("storing a chat's timer: {e}"));
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

    /// Only "this device is no longer linked" stops receiving for good; anything else is retried.
    #[test]
    fn an_unlinked_device_is_told_apart_from_a_network_error() {
        use presage::libsignal_service::prelude::ServiceError;
        use reqwest_websocket::HandshakeError;
        let handshake = |status: u16| {
            ServiceError::WsError(Box::new(reqwest_websocket::Error::Handshake(
                HandshakeError::UnexpectedStatusCode(http::StatusCode::from_u16(status).unwrap()),
            )))
        };
        assert!(super::is_unlinked(&ServiceError::Unauthorized));
        assert!(super::is_unlinked(&handshake(401)));
        assert!(super::is_unlinked(&handshake(403)));
        assert!(!super::is_unlinked(&handshake(500)));
        assert!(!super::is_unlinked(&handshake(429)));
        assert!(!super::is_unlinked(&ServiceError::WsClosing { reason: "gone" }));
    }

    /// "Retry now" and the network coming back cut the wait before reconnecting short.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_request_ends_the_wait_at_once() {
        let wake = tokio::sync::Notify::new();
        let started = tokio::time::Instant::now();
        assert!(
            !super::pause(std::time::Duration::from_secs(30), &wake).await,
            "waited it out"
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
        let started = tokio::time::Instant::now();
        let (woken, ()) = tokio::join!(super::pause(std::time::Duration::from_secs(30), &wake), async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            wake.notify_waiters();
        });
        assert!(woken);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// A request while nothing waits (connecting, handling a message) isn't kept: it would
    /// later drop a fresh connection, and with it messages the server counts as delivered.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_request_is_never_kept_for_later() {
        let wake = tokio::sync::Notify::new();
        wake.notify_waiters();
        let started = tokio::time::Instant::now();
        assert!(!super::pause(std::time::Duration::from_secs(5), &wake).await);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(5));
    }

    /// Each connection starts by catching up; it is live only once the server queue is drained.
    #[test]
    fn a_connection_is_live_only_after_draining_the_queue() {
        let mut p = super::Progress::default();
        p.connected();
        assert!(!p.live);
        assert!(p.drained(), "the first drain syncs names");
        assert!(p.live);
        p.connected(); // reconnect: catching up again
        assert!(!p.live);
        assert!(!p.drained(), "names are synced once");
        assert!(p.live);
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
                kind: crate::projection::MessageKind::Normal,
                expires_in: 0,
                expires_at: None,
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
        assert!(
            super::apply_received(&db, &Quiet, &[off], "me", "me", true)
                .await
                .settings_changed
        );
        assert_eq!(db.settings().await.unwrap().read_receipts, Some(false));
        let typing = Op::Typing {
            conversation_id: "c:a".into(),
            who: "a".into(),
            started: true,
        };
        assert!(
            !super::apply_received(&db, &Quiet, &[typing], "a", "me", true)
                .await
                .settings_changed
        );
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

    /// presage keeps its own copy of every message (and edit): an expired one goes from there too,
    /// and one that fails to go there is retried rather than forgotten.
    #[tokio::test]
    async fn an_expired_message_leaves_presages_store_too() {
        use presage::libsignal_service::content::{Content, ContentBody, Metadata};
        use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
        use presage::model::identity::OnNewIdentity;
        use presage::proto::DataMessage;
        use presage::store::{ContentsStore, Thread};
        use presage_store_sqlite::SqliteStore;

        const ALICE: &str = "22222222-2222-2222-2222-222222222222";
        let dir = std::env::temp_dir().join(format!("sig-core-expire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = "00".repeat(32);
        let mut store = SqliteStore::open_with_options(
            super::sqlite_options(&dir.join("presage.db"), &key).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        let alice = ServiceId::from(Aci::from(
            presage::libsignal_service::prelude::Uuid::parse_str(ALICE).unwrap(),
        ));
        let thread = Thread::Contact(alice);
        let at = |ms: u64| Content {
            metadata: Metadata {
                sender: alice,
                destination: alice,
                sender_device: DeviceId::new(1).unwrap(),
                pni_verified: None,
                client_timestamp: chrono::DateTime::from_timestamp_millis(ms as i64).unwrap(),
                server_timestamp: chrono::DateTime::from_timestamp_millis(ms as i64).unwrap(),
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body: ContentBody::DataMessage(DataMessage {
                timestamp: Some(ms),
                ..Default::default()
            }),
        };
        for ms in [100, 200] {
            store.save_message(&thread, at(ms)).await.unwrap();
        }
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap();
        let expired = |conversation_id: String, sent_at| crate::projection::Message {
            conversation_id,
            author: ALICE.into(),
            sent_at,
            received_at: sent_at,
            body: Some("x".into()),
            is_outgoing: false,
            status: crate::projection::MessageStatus::Read,
            edited: false,
            deleted: false,
            quote: None,
            kind: crate::projection::MessageKind::Normal,
            expires_in: 1,
            expires_at: Some(1),
        };
        db.insert_message(&expired(format!("c:{ALICE}"), 100)).await.unwrap();
        // a copy presage can't be asked to delete (a bad id) stays queued and fails the sweep
        db.insert_message(&expired("c:not-a-uuid".into(), 5)).await.unwrap();
        assert!(super::sweep(&mut store, &db, &Quiet).await.is_err());
        assert!(store.message(&thread, 100).await.unwrap().is_none());
        assert!(store.message(&thread, 200).await.unwrap().is_some(), "others stay");
        let left = vec![("c:not-a-uuid".to_string(), 5)];
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            left,
            "kept, for the next sweep"
        );
        assert!(super::sweep(&mut store, &db, &Quiet).await.is_err(), "retried");
        assert_eq!(db.presage_forget_queue().await.unwrap(), left);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
