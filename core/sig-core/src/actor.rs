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
use presage::libsignal_service::groups_v2::GroupInviteLink;
use presage::libsignal_service::protocol::{Aci, ServiceId, SignalProtocolError};
use presage::libsignal_service::push_service::ServiceIds;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::{ContentsStore, Store as _, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use tokio::sync::{mpsc, oneshot, watch};

use crate::download::{downloader, Download, Downloads};
use crate::error::CoreError;
use crate::events::{alert, from_applied, ConnectionState, CoreEvent, EventListener};
use crate::projection::ids::{direct_id, group_id};
use crate::projection::{project, ChatEventKind, GroupStanding, Op, ProjectionDb};
use crate::send::{sender, Job, Jobs};
use crate::storage;
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
    /// A call action from the app, forwarded to the call engine (spawned on first use).
    Call(crate::calls::actor::CallCommand),
    /// Stops the history import (or clears a failed one) and continues without it; receiving
    /// starts.
    CancelHistory,
    /// Stops the core so the app can delete presage's store and link again for history. Tells
    /// the server to drop the old registration, best effort.
    Relink {
        done: oneshot::Sender<()>,
    },
    /// Stops the core if it isn't linked (true), for restoring a backup; a linked one (the code
    /// was scanned meanwhile) keeps running (false).
    StopUnlinked {
        done: oneshot::Sender<bool>,
    },
    /// A chat setting changed here (or Storage Service writes were turned on): sync soon.
    StorageSync,
    /// Who has `username` ("name.42"): their ACI, None if no one, an error if it can't be asked.
    LookupUsername {
        username: String,
        done: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// The installed sticker packs (images written under `stickers/`), each with its key and its
    /// stickers' content types.
    StickerPacks {
        done: oneshot::Sender<Result<Vec<crate::projection::model::StickerPackInfo>, String>>,
    },
    /// One installed sticker, to send: None if its pack isn't installed or it has no image.
    Sticker {
        pack_id: Vec<u8>,
        sticker_id: u32,
        done: oneshot::Sender<Result<Option<ChosenSticker>, String>>,
    },
    /// Download and install a sticker pack (our other devices are told), then answer.
    InstallStickerPack {
        id: Vec<u8>,
        key: Vec<u8>,
        done: oneshot::Sender<Result<(), String>>,
    },
    /// A group as its info panel shows it, as presage has it; None if presage hasn't it.
    GroupInfo {
        key: [u8; 32],
        done: oneshot::Sender<Result<Option<crate::groups::GroupInfo>, String>>,
    },
    /// Changes a group (its members are told), then stores it in its chat.
    ChangeGroup {
        key: [u8; 32],
        edit: presage::manager::GroupEdit,
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Makes a group (its members are told), then lists its chat; answers its master key.
    CreateGroup {
        title: String,
        members: Vec<Aci>,
        done: oneshot::Sender<Result<[u8; 32], String>>,
    },
    /// What a group's invite link shows (our own copy if we're in it).
    PreviewGroupLink {
        link: GroupInviteLink,
        done: oneshot::Sender<Result<crate::groups::GroupPreview, String>>,
    },
    /// Joins a group through its link, or asks to, then lists its chat; answers whether we asked.
    JoinGroup {
        link: GroupInviteLink,
        done: oneshot::Sender<Result<bool, String>>,
    },
    /// Withdraws our request to join a group, then stores it in its chat.
    CancelJoinRequest {
        key: [u8; 32],
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Our profile as the server has it now (the phone changes it too).
    OwnProfile {
        done: oneshot::Sender<Result<crate::profile::OwnProfile, String>>,
    },
    /// Writes our name and about (the picture, the about's emoji, the payment address and the
    /// phone number setting kept), then tells our other devices.
    SetProfile {
        name: presage::libsignal_service::profile_name::ProfileName<String>,
        about: Option<String>,
        done: oneshot::Sender<Result<(), String>>,
    },
}

/// A sticker as the client needs it to send it.
pub(crate) struct ChosenSticker {
    pub pack_key: Vec<u8>,
    pub info: crate::projection::model::StickerInfo,
    /// presage's, else empty.
    pub content_type: String,
}

/// Downloading a pack's manifest and every sticker in it.
const STICKER_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

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
    // The thread sends itself Start when a history import ends. Weak, or the thread would hold
    // its own channel open and never see it close (the Client's drop would join forever).
    let self_commands = commands.downgrade();
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
                Box::pin(run(
                    data_dir,
                    key,
                    listener,
                    linked,
                    own_aci,
                    rx,
                    self_commands,
                    ready_tx,
                )),
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
                message: "the core stopped unexpectedly; restart Hush".into(),
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

#[allow(clippy::too_many_arguments)]
async fn run(
    dir: PathBuf,
    key: String,
    listener: Arc<dyn EventListener>,
    linked: Arc<AtomicBool>,
    own_aci: Arc<Mutex<Option<String>>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    commands: mpsc::WeakUnboundedSender<Command>,
    ready: oneshot::Sender<Result<ProjectionDb, CoreError>>,
) {
    let opened = async {
        // ProjectionDb::open validates the key format before anything is interpolated.
        let projection = ProjectionDb::open(&format!("sqlite://{}", dir.join("projection.db").display()), Some(&key))
            .await?
            .with_files(dir.join("attachments"));
        match projection.reroot_avatars().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "chat pictures now point into the moved data folder"),
            Err(e) => tracing::warn!(error = %e, "re-pointing chat pictures after a folder move failed"),
        }
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
    // Received call messages route to the call router below.
    let (incoming_calls_tx, incoming_calls_rx) = mpsc::unbounded_channel::<crate::calls::IncomingEnvelope>();
    // Storage Service sync triggers go to the single sync task below.
    let (storage_tx, storage_rx) = mpsc::unbounded_channel::<storage::Trigger>();
    let queues = Queues {
        sends: send_tx.clone(),
        downloads: download_tx.clone(),
        calls: incoming_calls_tx,
        storage: storage_tx.clone(),
    };
    // What the call engine needs from the link: our device id, ACI, and credentials.
    let registration: Rc<RefCell<Option<presage::manager::RegistrationData>>> =
        Rc::new(RefCell::new(manager.as_ref().map(|m| m.registration_data().clone())));
    let (call_out_tx, call_out_rx) = mpsc::unbounded_channel::<crate::calls::actor::CallOutput>();
    let call_commands = spawn_call_router(
        registration.clone(),
        store.clone(),
        projection.clone(),
        listener.clone(),
        send_tx.clone(),
        call_out_tx,
        call_out_rx,
        incoming_calls_rx,
    );
    spawn_storage_sync(
        registration.clone(),
        store.clone(),
        projection.clone(),
        listener.clone(),
        send_tx.clone(),
        storage_rx,
    );
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
    // The history import between linking and receiving, if the phone offered one.
    let history_running = Rc::new(Cell::new(false));
    let history_cancel: Rc<RefCell<watch::Sender<bool>>> = Rc::new(RefCell::new(watch::channel(false).0));
    // commands that came while a link code waited to be scanned
    let mut queued = std::collections::VecDeque::new();
    loop {
        let cmd = match queued.pop_front() {
            Some(cmd) => cmd,
            None => match rx.recv().await {
                Some(cmd) => cmd,
                None => break,
            },
        };
        match cmd {
            Command::Link { device_name } => {
                if manager.is_some() {
                    listener.on_event(CoreEvent::Error {
                        message: "already linked".into(),
                    });
                    continue;
                }
                // boxed to keep the large provisioning future off the stack (the stack size is the real fix)
                let linking = Box::pin(link(&store, device_name, listener.as_ref()));
                let linked_or = match race_link(linking, &mut rx, &mut queued).await {
                    Raced::Linked(r) => r,
                    Raced::Stopped | Raced::Closed => return,
                };
                match linked_or {
                    Ok(m) => {
                        // the key is one-time and lives only in memory: keep it (encrypted)
                        // before anything else, or a crash loses the offered history
                        if let Some(key) = m.registration_data().ephemeral_backup_key() {
                            let state = crate::projection::HistoryImportState {
                                stage: crate::projection::HistoryDbStage::Waiting,
                                key: key.to_vec(),
                                aci: m.registration_data().service_ids.aci.to_string(),
                                cdn: None,
                                cdn_key: None,
                                bytes_done: 0,
                                total_bytes: None,
                            };
                            if let Err(e) = projection.set_history_state(&state).await {
                                listener.on_event(CoreEvent::Error {
                                    message: format!("keeping the history key failed: {e}"),
                                });
                            }
                        }
                        set_aci(&m);
                        *registration.borrow_mut() = Some(m.registration_data().clone());
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
                    // a pending history import goes first: the server queues new messages
                    // meanwhile, so they arrive after the history, in order
                    match projection.history_state().await {
                        Ok(Some(_)) => {
                            if !history_running.get() {
                                start_history(
                                    m,
                                    &projection,
                                    &listener,
                                    &dir,
                                    &commands,
                                    &history_running,
                                    &history_cancel,
                                );
                            }
                            continue;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            // receiving must not start over a pending import: the user can
                            // Retry (Start) or Continue Without History
                            listener.on_event(CoreEvent::Error {
                                message: format!("checking the history import failed: {e}"),
                            });
                            listener.on_event(CoreEvent::HistoryProgress {
                                stage: crate::history::HistoryStage::Failed,
                                done: 0,
                                total: 0,
                            });
                            continue;
                        }
                    }
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
            Command::StorageSync => {
                let _ = storage_tx.send(storage::Trigger::Local);
            }
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
                            .add_chat_event(&conversation_id, at, ChatEventKind::SessionReset, &author, None)
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
            Command::Call(cmd) => match call_commands() {
                Some(tx) => {
                    if tx.send(cmd).is_err() {
                        listener.on_event(CoreEvent::Error {
                            message: "the call engine stopped; restart Hush to call".into(),
                        });
                    }
                }
                None => listener.on_event(CoreEvent::Error {
                    message: "calls need a linked phone".into(),
                }),
            },
            Command::CancelHistory => {
                // a send fails exactly when the task died: fall through and clean up here;
                // a live task cleans up and starts receiving itself
                let task_listens = history_running.get() && history_cancel.borrow().send(true).is_ok();
                if !task_listens {
                    // a failed, dead, or never started import: clear it here
                    if let Err(e) = projection.clear_history_state().await {
                        listener.on_event(CoreEvent::Error {
                            message: format!("clearing the history import failed: {e}"),
                        });
                    }
                    let _ = std::fs::remove_file(dir.join("history.tmp"));
                    listener.on_event(CoreEvent::HistoryProgress {
                        stage: crate::history::HistoryStage::NoHistory,
                        done: 0,
                        total: 0,
                    });
                    if manager.is_some() {
                        if let Some(c) = commands.upgrade() {
                            let _ = c.send(Command::Start);
                        }
                    }
                }
            }
            Command::StickerPacks { done } => match &manager {
                Some(m) => {
                    let (store, dir) = (m.store().clone(), dir.join("stickers"));
                    tokio::task::spawn_local(async move {
                        let _ = done.send(installed_packs(&store, dir).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::Sticker {
                pack_id,
                sticker_id,
                done,
            } => match &manager {
                Some(m) => {
                    let (store, dir) = (m.store().clone(), dir.join("stickers"));
                    tokio::task::spawn_local(async move {
                        let _ = done.send(chosen_sticker(&store, dir, pack_id, sticker_id).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::InstallStickerPack { id, key, done } => match &manager {
                Some(m) => {
                    // a pack can take a while: commands go on meanwhile
                    let mut m = m.clone();
                    tokio::task::spawn_local(async move {
                        let _ = done.send(install_pack(&mut m, &id, &key).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::LookupUsername { username, done } => {
                let found = match &mut manager {
                    Some(m) => {
                        match tokio::time::timeout(Duration::from_secs(15), Box::pin(m.lookup_username(&username)))
                            .await
                        {
                            Ok(Ok(aci)) => Ok(aci.map(|a| ServiceId::from(a).raw_uuid().to_string())),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => Err("the server didn't answer".into()),
                        }
                    }
                    None => Err("not linked yet".into()),
                };
                let _ = done.send(found);
            }
            Command::GroupInfo { key, done } => {
                let info = match &manager {
                    Some(m) => {
                        let me = m.registration_data().service_ids.aci.to_string();
                        match m.store().group(key).await {
                            Ok(g) => Ok(g.map(|g| crate::groups::info(&g, key, &me))),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                    None => Err("not linked yet".into()),
                };
                let _ = done.send(info);
            }
            Command::ChangeGroup { key, edit, done } => match &manager {
                Some(m) => {
                    // the server, then a message to every member: commands go on meanwhile
                    let (mut m, db, l) = (m.clone(), projection.clone(), listener.clone());
                    tokio::task::spawn_local(async move {
                        let _ = done.send(change_group(&mut m, &db, l.as_ref(), key, edit).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::CreateGroup { title, members, done } => match &manager {
                Some(m) => {
                    let (mut m, db, l) = (m.clone(), projection.clone(), listener.clone());
                    tokio::task::spawn_local(async move {
                        let _ = done.send(create_group(&mut m, &db, l.as_ref(), &title, &members).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::PreviewGroupLink { link, done } => match &manager {
                Some(m) => {
                    let mut m = m.clone();
                    tokio::task::spawn_local(async move {
                        let _ = done.send(preview_link(&mut m, &link).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::JoinGroup { link, done } => match &manager {
                Some(m) => {
                    let (mut m, db, l) = (m.clone(), projection.clone(), listener.clone());
                    tokio::task::spawn_local(async move {
                        let _ = done.send(join_group(&mut m, &db, l.as_ref(), &link).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::CancelJoinRequest { key, done } => match &manager {
                Some(m) => {
                    let (mut m, db, l) = (m.clone(), projection.clone(), listener.clone());
                    tokio::task::spawn_local(async move {
                        let cancelled =
                            match tokio::time::timeout(Duration::from_secs(30), Box::pin(m.cancel_join_request(&key)))
                                .await
                            {
                                Ok(r) => r.map_err(|e| crate::groups::without_link(e.to_string())),
                                Err(_) => Err("the server didn't answer in time".into()),
                            };
                        let own = m.registration_data().service_ids.clone();
                        settle_group(m.store(), &own, &db, l.as_ref(), &key, None).await;
                        let _ = done.send(cancelled);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::OwnProfile { done } => match &manager {
                Some(m) => {
                    let m = m.clone();
                    tokio::task::spawn_local(async move {
                        let _ = done.send(fetch_own_profile(&m).await.map(|(p, _)| crate::profile::own(&p)));
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::SetProfile { name, about, done } => match &manager {
                Some(m) => {
                    let (mut m, l) = (m.clone(), listener.clone());
                    tokio::task::spawn_local(async move {
                        let _ = done.send(set_profile(&mut m, l.as_ref(), name, about).await);
                    });
                }
                None => {
                    let _ = done.send(Err("not linked yet".into()));
                }
            },
            Command::StopUnlinked { done } => {
                if manager.is_some() {
                    let _ = done.send(false);
                    continue;
                }
                let _ = done.send(true);
                return;
            }
            Command::Relink { done } => {
                if let Some(m) = &manager {
                    // Best effort: the new link works either way, but each failure leaves a
                    // ghost entry on the account (Signal caps linked devices), so say so.
                    if tokio::time::timeout(Duration::from_secs(10), Box::pin(m.unlink_self()))
                        .await
                        .map_or(true, |r| r.is_err())
                    {
                        tracing::warn!("the server couldn't be told about the relink");
                        listener.on_event(CoreEvent::Error {
                            message: "The old link couldn't be removed; if linking ever fails, remove                                       this Mac's old entry on your phone under Linked Devices."
                                .into(),
                        });
                    }
                }
                linked.store(false, Ordering::SeqCst);
                let _ = done.send(());
                // the thread ends; the Client deletes presage's store and the app relinks
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

/// Runs the history import on this thread; receiving waits for it. When it ends (imported, none
/// offered, or cancelled) it starts receiving by sending `Start`; a failure waits for the user
/// (Retry sends `Start` again, Continue Without History sends `CancelHistory`).
fn start_history(
    m: &Manager<SqliteStore, Registered>,
    projection: &ProjectionDb,
    listener: &Arc<dyn EventListener>,
    dir: &std::path::Path,
    commands: &mpsc::WeakUnboundedSender<Command>,
    running: &Rc<Cell<bool>>,
    cancel: &Rc<RefCell<watch::Sender<bool>>>,
) {
    running.set(true);
    let (tx, mut rx) = watch::channel(false);
    *cancel.borrow_mut() = tx;
    let running2 = running.clone();
    let (m, db, l, commands, running) = (
        m.clone(),
        projection.clone(),
        listener.clone(),
        commands.clone(),
        running.clone(),
    );
    let tmp = dir.join("history.tmp");
    // (A CancelHistory racing the task's end can emit NoHistory after Done and a second Start;
    // both are harmless: the app ignores stage order after a terminal one, Start is idempotent.)
    let task = tokio::task::spawn_local(async move {
        let mut store = m.store().clone();
        let outcome = async {
            let mut server = crate::history::fetch::SignalServer::from_registration(m.registration_data())?;
            crate::history::run(&db, &mut store, &mut server, &tmp, l.as_ref(), &mut rx).await
        }
        .await;
        running.set(false);
        match outcome {
            Ok(outcome) => {
                // imported, none offered, or cancelled: live messages may flow now
                if let Some(c) = commands.upgrade() {
                    let _ = c.send(Command::Start);
                }
                // then the phone's sticker packs, one at a time; a pack that fails is left out
                if let crate::history::Outcome::Done { sticker_packs } = outcome {
                    let mut m = m;
                    for (id, key) in sticker_packs {
                        if let Err(e) = install_pack(&mut m, &id, &key).await {
                            tracing::warn!(error = %e, "a sticker pack from the phone wasn't installed");
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("the history transfer failed: {e}");
                l.on_event(CoreEvent::Error {
                    message: format!("The history transfer failed: {e}"),
                });
                l.on_event(CoreEvent::HistoryProgress {
                    stage: crate::history::HistoryStage::Failed,
                    done: 0,
                    total: 0,
                });
            }
        }
    });
    // Supervise: a panic in the import must not leave the overlay stuck and receiving gated
    // forever with no way out.
    let (running, l) = (running2, listener.clone());
    tokio::task::spawn_local(async move {
        if task.await.is_err() {
            running.set(false);
            l.on_event(CoreEvent::Error {
                message: "the history import stopped unexpectedly".into(),
            });
            l.on_event(CoreEvent::HistoryProgress {
                stage: crate::history::HistoryStage::Failed,
                done: 0,
                total: 0,
            });
        }
    });
}

/// The installed packs with stickers we have images of, by title (images written off this thread).
async fn installed_packs(
    store: &SqliteStore,
    dir: PathBuf,
) -> Result<Vec<crate::projection::model::StickerPackInfo>, String> {
    let mut packs = vec![];
    for pack in store.sticker_packs().await.map_err(|e| e.to_string())? {
        let pack = match pack {
            Ok(p) => p,
            Err(e) => {
                // one unreadable pack doesn't hide the others
                tracing::warn!(error = %e, "a sticker pack couldn't be read");
                continue;
            }
        };
        packs.push(pack);
    }
    let mut infos = tokio::task::spawn_blocking(move || {
        packs
            .iter()
            .map(|p| crate::stickers::pack_info(p, &dir))
            .filter(|i| !i.stickers.is_empty())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    infos.sort_by_key(|i| i.title.to_lowercase());
    Ok(infos)
}

/// One sticker from its pack alone (not every pack), its image written off this thread.
async fn chosen_sticker(
    store: &SqliteStore,
    dir: PathBuf,
    pack_id: Vec<u8>,
    sticker_id: u32,
) -> Result<Option<ChosenSticker>, String> {
    let Some(pack) = store.sticker_pack(&pack_id).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        let s = pack.manifest.stickers.iter().find(|s| s.id == sticker_id)?;
        Some(ChosenSticker {
            info: crate::stickers::sticker_info(&pack, s, &dir)?,
            content_type: s.content_type.clone().unwrap_or_default(),
            pack_key: pack.key.clone(),
        })
    })
    .await
    .map_err(|e| e.to_string())
}

/// Makes a group change, then stores the group in its chat.
async fn change_group(
    m: &mut Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    l: &dyn EventListener,
    key: [u8; 32],
    edit: presage::manager::GroupEdit,
) -> Result<(), String> {
    use presage::manager::GroupEdit;
    let me = m.registration_data().service_ids.aci.to_string();
    // removing ourselves would skip presage's handling of leaving
    if matches!(&edit, GroupEdit::RemoveMember(a) | GroupEdit::SetAdmin(a, _) if a.service_id_string() == me) {
        return Err("that change can't be made to yourself".into());
    }
    let picture = match &edit {
        GroupEdit::Avatar(bytes) => Some(bytes.clone()),
        _ => None,
    };
    let answer = match &edit {
        GroupEdit::AcceptInvitation => Some(true),
        GroupEdit::DeclineInvitation => Some(false),
        _ => None,
    };
    let changed = match tokio::time::timeout(Duration::from_secs(60), Box::pin(m.change_group(&key, edit))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the server didn't answer in time; the change may still have been made".into()),
    };
    // whatever the answer: the server may have taken the change before something after it failed
    // (telling the members, say), and presage then has the group as changed
    let picture = picture.filter(|_| changed.is_ok());
    let own = m.registration_data().service_ids.clone();
    settle_group(m.store(), &own, db, l, &key, picture.as_deref()).await;
    // an answer to an invitation that took is answered, though the members weren't told
    if let (Err(problem), Some(accept)) = (&changed, answer) {
        if let Ok(Some(g)) = m.store().group(key).await {
            if crate::groups::answered(&g, own.aci, own.pni, accept) {
                l.on_event(CoreEvent::Error {
                    message: format!(
                        "Your answer to the invitation went through, but the group may not know yet: {problem}"
                    ),
                });
                return Ok(());
            }
        }
    }
    changed
}

/// Stores presage's group in its chat after a change, and the picture we set (empty: removed),
/// which presage keeps to itself.
async fn settle_group(
    store: &SqliteStore,
    own: &ServiceIds,
    db: &ProjectionDb,
    l: &dyn EventListener,
    key: &[u8; 32],
    picture: Option<&[u8]>,
) {
    let mut changed = false;
    let mut problems = vec![];
    match store.group(*key).await {
        Ok(Some(g)) => match store_group(db, key, &g, own).await {
            Ok(c) => changed |= c,
            Err(e) => problems.push(format!("storing a changed group failed: {e}")),
        },
        Ok(None) => {}
        Err(e) => problems.push(format!("reading a changed group failed: {e}")),
    }
    if let Some(bytes) = picture {
        let now = chrono::Utc::now().timestamp_millis();
        match db
            .store_avatar(&group_id(key), (!bytes.is_empty()).then_some(bytes), now)
            .await
        {
            Ok(c) => changed |= c,
            Err(e) => problems.push(format!("storing a group's new picture failed: {e}")),
        }
    }
    for message in problems {
        l.on_event(CoreEvent::Error { message });
    }
    if changed {
        l.on_event(CoreEvent::ConversationsChanged);
    }
}

/// Makes a group, then lists its chat. A failure after the server made it (telling the
/// members, say) still lists and answers it, the failure going out as an error event.
async fn create_group(
    m: &mut Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    l: &dyn EventListener,
    title: &str,
    members: &[Aci],
) -> Result<[u8; 32], String> {
    let before = group_keys(m.store()).await?;
    match tokio::time::timeout(Duration::from_secs(60), Box::pin(m.create_group(title, members))).await {
        Ok(Ok((key, at))) => {
            let own = m.registration_data().service_ids.clone();
            list_new_group(m.store(), &own, db, l, &key, at as i64).await;
            Ok(key)
        }
        failed => {
            let problem = match failed {
                Ok(Err(e)) => e.to_string(),
                // dropped midway, perhaps after the server made it but before presage stored it
                _ => "the server didn't answer in time; the group may have been made, so look in your chats before trying again".into(),
            };
            let me = Aci::from(m.registration_data().service_ids.aci);
            match our_new_group(m.store(), &before, &me, title, members).await {
                Ok(Some(key)) => {
                    let own = m.registration_data().service_ids.clone();
                    let now = chrono::Utc::now().timestamp_millis();
                    list_new_group(m.store(), &own, db, l, &key, now).await;
                    l.on_event(CoreEvent::Error {
                        message: format!("The group was made, but its members may not know yet: {problem}"),
                    });
                    Ok(key)
                }
                Ok(None) => Err(problem),
                Err(e) => Err(format!(
                    "{problem}; the group may have been made, so look in your chats before trying again \
                     (reading the groups failed: {e})"
                )),
            }
        }
    }
}

/// The group we just made, if presage has it though making it failed: the one group it didn't
/// have `before`, at its first revision, under our `title`, with us (`me`) its only admin and
/// everyone else one of the `members` we asked for. A group someone else made and added us to
/// meanwhile has its maker for admin.
async fn our_new_group(
    store: &SqliteStore,
    before: &std::collections::HashSet<[u8; 32]>,
    me: &Aci,
    title: &str,
    members: &[Aci],
) -> Result<Option<[u8; 32]>, String> {
    use presage::libsignal_service::groups_v2::Role;
    use presage::libsignal_service::prelude::Uuid;
    let Some(key) = made_group(before, &group_keys(store).await?) else {
        return Ok(None);
    };
    let asked = |uuid: Uuid| members.iter().any(|a| ServiceId::from(*a).raw_uuid() == uuid);
    let ours = |g: &presage::model::groups::Group| {
        g.title == title
            && g.revision == 0
            && g.members.iter().any(|m| m.aci == *me)
            && g.members.iter().all(|m| match m.aci == *me {
                true => m.role == Role::Administrator,
                false => m.role != Role::Administrator && asked(ServiceId::from(m.aci).raw_uuid()),
            })
            && g.pending_members.iter().all(|p| asked(p.uuid))
    };
    let group = store.group(key).await.map_err(|e| e.to_string())?;
    Ok(group.filter(ours).map(|_| key))
}

/// One call on our profile over HTTPS, 20 s at most: the answer's body on a 200.
async fn profile_call(
    m: &Manager<SqliteStore, Registered>,
    method: reqwest::Method,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<Vec<u8>, String> {
    use presage::libsignal_service::configuration::Endpoint;
    use presage::libsignal_service::push_service::HttpAuthOverride;
    let push = crate::net::push_service(m.registration_data())?;
    let mut request = push
        .request(method, Endpoint::service(path), HttpAuthOverride::NoOverride)
        .map_err(|e| e.to_string())?;
    if let Some(body) = body {
        request = request.json(&body);
    }
    let call = async {
        // reqwest's error text carries the URL, our ACI in it: not for the log
        let response = request.send().await.map_err(|e| e.without_url().to_string())?;
        match response.status().as_u16() {
            200 => Ok(response
                .bytes()
                .await
                .map_err(|e| e.without_url().to_string())?
                .to_vec()),
            status => Err(format!("the server answered {status}")),
        }
    };
    tokio::time::timeout(Duration::from_secs(20), call)
        .await
        .map_err(|_| "the server didn't answer in time".to_string())?
}

/// Our profile as the server has it now, and what a write must send back. presage answers ours
/// from a copy nothing refreshes, and fetching it anew through presage's websocket isn't
/// offered (a second one would displace it): it's asked over HTTPS, and presage's copy replaced.
async fn fetch_own_profile(
    m: &Manager<SqliteStore, Registered>,
) -> Result<(presage::libsignal_service::Profile, crate::profile::Kept), String> {
    let data = m.registration_data();
    let (aci, key) = (data.service_ids.aci(), data.profile_key());
    let body = profile_call(m, reqwest::Method::GET, crate::profile::path(aci, key), None).await?;
    let (profile, kept) = (crate::profile::decrypt(&body, key)?, crate::profile::kept(&body)?);
    // what presage answers from its copy (Note to Self's picture) follows
    if let Err(e) = m.store().clone().save_profile(aci.into(), key, profile.clone()).await {
        tracing::warn!(error = %e, "our fetched profile wasn't stored");
    }
    Ok((profile, kept))
}

async fn set_profile(
    m: &mut Manager<SqliteStore, Registered>,
    l: &dyn EventListener,
    name: presage::libsignal_service::profile_name::ProfileName<String>,
    about: Option<String>,
) -> Result<(), String> {
    // the emoji and what we keep as they are now: the phone may have changed them since
    let (current, kept) = fetch_own_profile(m).await?;
    let emoji = crate::profile::kept_emoji(&about, current.about_emoji);
    let data = m.registration_data();
    let body = crate::profile::write(data.service_ids.aci(), data.profile_key(), &name, &about, &emoji, &kept)?;
    let written = profile_call(m, reqwest::Method::PUT, "/v1/profile".into(), Some(body)).await;
    // read back either way: presage's copy follows, and a write whose answer was lost may have taken
    match (written, fetch_own_profile(m).await) {
        (Ok(_), _) => {}
        (Err(e), Ok((p, _))) if crate::profile::took(&p, &name, &about) => {
            tracing::warn!(error = %e, "saving our profile answered an error, but it took");
        }
        (Err(e), _) => return Err(e),
    }
    // as Signal Desktop does, so the phone shows (and keeps) the new name
    let me = m.registration_data().service_ids.aci();
    let sync = crate::send::local_profile_changed_sync();
    let told = tokio::time::timeout(
        Duration::from_secs(30),
        Box::pin(m.send_message(me, sync, chrono::Utc::now().timestamp_millis() as u64)),
    )
    .await;
    let failed = match told {
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some("timed out".to_string()),
    };
    if let Some(e) = failed {
        tracing::warn!(error = %e, "the phone wasn't told our profile changed");
        l.on_event(CoreEvent::Error {
            message: "Your profile is saved, but your phone wasn't told: it may show the old one for a while.".into(),
        });
    }
    Ok(())
}

/// What a group's link shows: the group as the server has it for us if we're in it (our copy
/// may say so though we were removed: it's fetched again), else the server's answer to
/// whoever has the link.
async fn preview_link(
    m: &mut Manager<SqliteStore, Registered>,
    link: &GroupInviteLink,
) -> Result<crate::groups::GroupPreview, String> {
    let id = group_id(&link.master_key);
    let me = m.registration_data().service_ids.aci;
    let ours = m.store().group(link.master_key).await.map_err(|e| e.to_string())?;
    if ours.is_some_and(|g| crate::groups::is_member(&g, me)) {
        // a removed member can't fetch it: then they're asked as anyone with the link
        if let Ok(Ok(g)) =
            tokio::time::timeout(Duration::from_secs(20), Box::pin(m.fetch_group(&link.master_key))).await
        {
            if crate::groups::is_member(&g, me) {
                return Ok(crate::groups::member_preview(&g, id));
            }
        }
    }
    let info = match tokio::time::timeout(Duration::from_secs(20), Box::pin(m.group_join_info(link))).await {
        Ok(info) => info.map_err(|e| crate::groups::without_link(e.to_string()))?,
        Err(_) => return Err("the server didn't answer in time".into()),
    };
    crate::groups::preview(&info, id)
}

/// Joins a group through its link, or asks to, then lists its chat; answers whether we asked.
/// A failure after the server took it (telling the members, say) still lists the chat and
/// answers, the failure going out as an error event: presage then has us in the group or
/// asking.
async fn join_group(
    m: &mut Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    l: &dyn EventListener,
    link: &GroupInviteLink,
) -> Result<bool, String> {
    use presage::manager::GroupJoin;
    let own = m.registration_data().service_ids.clone();
    let key = link.master_key;
    // where we stood before: a failure counts as joined only if that moved
    let standing = |g: Option<presage::model::groups::Group>| {
        g.and_then(|g| match crate::groups::is_member(&g, own.aci) {
            true => Some(false),
            false => (crate::groups::standing(&g, own.aci, own.pni) == GroupStanding::Requesting).then_some(true),
        })
    };
    let before = standing(m.store().group(key).await.map_err(|e| e.to_string())?);
    let joined = match tokio::time::timeout(Duration::from_secs(60), Box::pin(m.join_group(link))).await {
        Ok(Ok(GroupJoin::Joined { .. })) => Ok(false),
        Ok(Ok(GroupJoin::Requested)) => Ok(true),
        Ok(Err(e)) => Err(crate::groups::without_link(e.to_string())),
        Err(_) => Err("the server didn't answer in time".into()),
    };
    let now = chrono::Utc::now().timestamp_millis();
    match joined {
        Ok(requested) => {
            list_new_group(m.store(), &own, db, l, &key, now).await;
            Ok(requested)
        }
        Err(problem) => {
            let after = match m.store().group(key).await {
                Ok(g) => standing(g),
                Err(e) => return Err(format!("{problem}; reading the group failed: {e}")),
            };
            // presage's copy from before this attempt (an old request) isn't this join
            let Some(requested) = after.filter(|_| after != before) else {
                return Err(problem);
            };
            list_new_group(m.store(), &own, db, l, &key, now).await;
            l.on_event(CoreEvent::Error {
                message: format!("You're in, or asked to be, but the group may not know yet: {problem}"),
            });
            Ok(requested)
        }
    }
}

/// The master keys of the groups presage has (an unreadable one is left out: it isn't ours).
async fn group_keys(store: &SqliteStore) -> Result<std::collections::HashSet<[u8; 32]>, String> {
    let groups = store.groups().await.map_err(|e| e.to_string())?;
    Ok(groups.filter_map(|g| g.ok().map(|(key, _)| key)).collect())
}

/// The group made between two reads of presage's groups, if exactly one appeared.
fn made_group(
    before: &std::collections::HashSet<[u8; 32]>,
    after: &std::collections::HashSet<[u8; 32]>,
) -> Option<[u8; 32]> {
    let mut new = after.difference(before);
    match (new.next(), new.next()) {
        (Some(key), None) => Some(*key),
        _ => None,
    }
}

/// Stores a group we just made in its chat, listed from `at` though it has no messages yet.
async fn list_new_group(
    store: &SqliteStore,
    own: &ServiceIds,
    db: &ProjectionDb,
    l: &dyn EventListener,
    key: &[u8; 32],
    at: i64,
) {
    let listed = match store.group(*key).await {
        Ok(Some(g)) => match store_group(db, key, &g, own).await {
            Ok(_) => db.list_chat(&group_id(key), at).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        },
        Ok(None) => Err("presage hasn't it".into()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = listed {
        l.on_event(CoreEvent::Error {
            message: format!("listing a new group failed: {e}"),
        });
    }
    l.on_event(CoreEvent::ConversationsChanged);
}

/// Installs a pack unless presage has it already.
async fn install_pack(m: &mut Manager<SqliteStore, Registered>, id: &[u8], key: &[u8]) -> Result<(), String> {
    if matches!(m.store().sticker_pack(id).await, Ok(Some(_))) {
        return Ok(());
    }
    match tokio::time::timeout(STICKER_INSTALL_TIMEOUT, Box::pin(m.install_sticker_pack(id, key))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the download took too long".into()),
    }
}

/// Local changes come in bursts (a few clicks): one sync after they settle.
#[cfg(not(test))]
const STORAGE_DEBOUNCE: Duration = Duration::from_millis(500);
#[cfg(test)]
const STORAGE_DEBOUNCE: Duration = Duration::ZERO;

/// The Storage Service sync, one at a time: each trigger (connected, the phone's request, a
/// local change) runs a pull then push; triggers that arrive meanwhile fold into the next run.
/// Runs only while linked; survives relinks like the call router (same `registration`).
fn spawn_storage_sync(
    registration: Rc<RefCell<Option<presage::manager::RegistrationData>>>,
    store: SqliteStore,
    db: ProjectionDb,
    listener: Arc<dyn EventListener>,
    send_tx: mpsc::UnboundedSender<Job>,
    mut rx: mpsc::UnboundedReceiver<storage::Trigger>,
) {
    let l = listener.clone();
    let task = tokio::task::spawn_local(async move {
        while let Some(trigger) = rx.recv().await {
            if trigger == storage::Trigger::Local {
                tokio::time::sleep(STORAGE_DEBOUNCE).await;
            }
            while rx.try_recv().is_ok() {}
            let Some(data) = registration.borrow().clone() else {
                continue; // unlinked: the next connect syncs
            };
            let result = storage::sync_once(&store, &db, &data).await;
            match &result {
                Ok(Some(out)) => tracing::info!(
                    ?trigger,
                    applied = out.pulled.applied,
                    version = out.pulled.version,
                    written = out.pushed.as_ref().map_or(0, |p| p.written),
                    "storage synced"
                ),
                Ok(None) => {}
                Err(e) => tracing::warn!(?trigger, error = %e, "storage sync failed"),
            }
            let (events, wrote) = storage::effects(&result);
            for event in events {
                listener.on_event(event);
            }
            if wrote {
                let _ = send_tx.send(Job::StorageChanged);
            }
        }
    });
    // a panic must not end syncing unnoticed (the triggers' channel died with the task)
    tokio::task::spawn_local(async move {
        if let Err(e) = task.await {
            l.on_event(CoreEvent::Error {
                message: format!("Syncing chat settings with the phone stopped until Hush restarts: {e}"),
            });
        }
    });
}

/// Routes everything between the call engine's thread and the rest of the core: incoming
/// envelopes get identity keys and go in, outgoing envelopes go out through the sender with
/// acks, relay requests are fetched, chat events written. The engine thread starts on first
/// use (it opens the audio stack) and only while linked.
#[allow(clippy::too_many_arguments)]
fn spawn_call_router(
    registration: Rc<RefCell<Option<presage::manager::RegistrationData>>>,
    store: SqliteStore,
    db: ProjectionDb,
    listener: Arc<dyn EventListener>,
    send_tx: mpsc::UnboundedSender<Job>,
    call_out_tx: mpsc::UnboundedSender<crate::calls::actor::CallOutput>,
    mut call_out_rx: mpsc::UnboundedReceiver<crate::calls::actor::CallOutput>,
    mut incoming_rx: mpsc::UnboundedReceiver<crate::calls::IncomingEnvelope>,
) -> Rc<dyn Fn() -> Option<std::sync::mpsc::Sender<crate::calls::actor::CallCommand>>> {
    use crate::calls::actor::{CallCommand, CallOutput};
    let engine: Rc<RefCell<Option<std::sync::mpsc::Sender<CallCommand>>>> = Rc::new(RefCell::new(None));
    let ensure_engine = {
        let engine = engine.clone();
        let registration = registration.clone();
        let listener = listener.clone();
        move || -> Option<std::sync::mpsc::Sender<CallCommand>> {
            if let Some(tx) = engine.borrow().clone() {
                return Some(tx);
            }
            let reg = registration.borrow();
            let data = reg.as_ref()?;
            let device_id = data.device_id.unwrap_or(1);
            let tx = crate::calls::actor::spawn(device_id, listener.clone(), call_out_tx.clone());
            *engine.borrow_mut() = Some(tx.clone());
            Some(tx)
        }
    };
    // Incoming envelopes: attach identity keys (RingRTC's call fingerprint) and hand them in.
    {
        let ensure_engine = ensure_engine.clone();
        let (store, listener, registration) = (store.clone(), listener.clone(), registration.clone());
        tokio::task::spawn_local(async move {
            use presage::libsignal_service::protocol::{IdentityKeyStore, ProtocolAddress};
            while let Some(envelope) = incoming_rx.recv().await {
                let Some(sender_aci) = crate::calls::call_sender_aci(&envelope.sender) else {
                    tracing::warn!("a call message from a non-ACI sender was ignored");
                    continue;
                };
                let our_device_id = registration.borrow().as_ref().and_then(|d| d.device_id).unwrap_or(1);
                let calls = crate::calls::wire::from_proto(&envelope.message, our_device_id);
                if calls.is_empty() {
                    continue; // not for this device, or nothing we handle: no audio stack for it
                }
                let Some(engine) = ensure_engine() else {
                    tracing::warn!("a call message arrived while unlinked");
                    continue;
                };
                let store = store.aci_protocol_store();
                // RingRTC derives media keys from both identity keys; phones use the bare
                // 32-byte key (no type byte), so we must too
                let ours = match store.get_identity_key_pair().await {
                    Ok(pair) => pair.identity_key().public_key().public_key_bytes().to_vec(),
                    Err(e) => {
                        listener.on_event(CoreEvent::Error {
                            message: format!("a call couldn't be checked against our identity: {e}"),
                        });
                        continue;
                    }
                };
                let Some(device_id) = u8::try_from(envelope.sender_device)
                    .ok()
                    .and_then(|d| presage::libsignal_service::protocol::DeviceId::new(d).ok())
                else {
                    continue;
                };
                let address = ProtocolAddress::new(envelope.sender.service_id_string(), device_id);
                let theirs = match store.get_identity(&address).await {
                    Ok(Some(identity)) => identity.public_key().public_key_bytes().to_vec(),
                    Ok(None) => {
                        // no session with this device: a call can't be verified, so it can't ring
                        tracing::warn!(sender = %sender_aci, "a call from an unknown device was ignored");
                        continue;
                    }
                    Err(e) => {
                        listener.on_event(CoreEvent::Error {
                            message: format!("a call couldn't be checked against the sender's identity: {e}"),
                        });
                        continue;
                    }
                };
                let now = chrono::Utc::now().timestamp_millis();
                let age_secs = ((now - envelope.server_timestamp_ms).max(0) / 1000) as u64;
                let _ = engine.send(CallCommand::Incoming {
                    sender_aci,
                    sender_device_id: envelope.sender_device,
                    age_secs,
                    sender_identity_key: theirs,
                    receiver_identity_key: ours,
                    calls,
                });
            }
        });
    }
    // The engine's outputs: sends (with acks), relay fetches, chat events.
    {
        let engine = engine.clone();
        tokio::task::spawn_local(async move {
            while let Some(output) = call_out_rx.recv().await {
                match output {
                    CallOutput::Send {
                        peer_aci,
                        call_id_u64,
                        proto,
                    } => {
                        let (done, outcome) = tokio::sync::oneshot::channel();
                        let job = Job::Call {
                            conversation_id: format!("c:{peer_aci}"),
                            body: Box::new(presage::libsignal_service::content::ContentBody::CallMessage(proto)),
                            at: chrono::Utc::now().timestamp_millis() as u64,
                            done,
                        };
                        if send_tx.send(job).is_err() {
                            if let Some(tx) = engine.borrow().clone() {
                                let _ = tx.send(CallCommand::SendResult { call_id_u64, ok: false });
                            }
                            continue;
                        }
                        let engine = engine.clone();
                        tokio::task::spawn_local(async move {
                            let ok = outcome.await.unwrap_or(false);
                            if let Some(tx) = engine.borrow().clone() {
                                let _ = tx.send(CallCommand::SendResult { call_id_u64, ok });
                            }
                        });
                    }
                    CallOutput::NeedRelays { call_id_u64 } => {
                        let data = registration.borrow().clone();
                        let engine = engine.clone();
                        let listener = listener.clone();
                        tokio::task::spawn_local(async move {
                            let servers = match data.as_ref().map(crate::net::push_service) {
                                Some(Ok(push)) => crate::calls::relays::fetch(&push).await,
                                Some(Err(e)) => {
                                    tracing::warn!("no credentials for the relay fetch: {e}");
                                    Vec::new()
                                }
                                None => Vec::new(),
                            };
                            if servers.is_empty() {
                                listener.on_event(CoreEvent::Error {
                                    message:
                                        "calling relays unavailable (see the log); the call tries a direct connection"
                                            .into(),
                                });
                            }
                            if let Some(tx) = engine.borrow().clone() {
                                let _ = tx.send(CallCommand::Relays { call_id_u64, servers });
                            }
                        });
                    }
                    CallOutput::ChatEvent { peer_aci, kind, value } => {
                        let conversation_id = format!("c:{peer_aci}");
                        let at = chrono::Utc::now().timestamp_millis();
                        match db.add_chat_event(&conversation_id, at, kind, &peer_aci, value).await {
                            Ok(()) => {
                                listener.on_event(CoreEvent::ChatEventAdded { conversation_id });
                                listener.on_event(CoreEvent::ConversationsChanged);
                            }
                            Err(e) => listener.on_event(CoreEvent::Error {
                                message: format!("recording a call failed: {e}"),
                            }),
                        }
                    }
                }
            }
        });
    }
    // Commands from the app reach the engine through this: it spawns it on first use.
    Rc::new(ensure_engine)
}

/// Links as a secondary device: emits `LinkUrl`, waits (bounded) for the phone to scan, then asks
/// the phone for contacts. Returns a message for the user on failure.
/// How waiting for a link to be scanned ended.
#[derive(Debug, PartialEq)]
enum Raced<T> {
    Linked(T),
    /// A command stopped the core (it was answered).
    Stopped,
    /// The Client is gone.
    Closed,
}

/// Waits for `linking`, meanwhile answering the commands that stop an unlinked core (to restore
/// a backup, relink or unlink) and queuing the rest for after the link.
async fn race_link<T>(
    mut linking: std::pin::Pin<Box<impl std::future::Future<Output = T>>>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut std::collections::VecDeque<Command>,
) -> Raced<T> {
    loop {
        tokio::select! {
            r = &mut linking => return Raced::Linked(r),
            cmd = rx.recv() => match cmd {
                Some(Command::Relink { done }) => {
                    let _ = done.send(());
                    return Raced::Stopped;
                }
                Some(Command::StopUnlinked { done }) => {
                    let _ = done.send(true);
                    return Raced::Stopped;
                }
                Some(Command::Unlink { done, .. }) => {
                    let _ = done.send(Ok(()));
                    return Raced::Stopped;
                }
                Some(cmd) => queued.push_back(cmd),
                None => return Raced::Closed,
            },
        }
    }
}

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
    // a day-old story leaves the list (and an open viewer)
    if db
        .expire_stories(chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|e| e.to_string())?
    {
        l.on_event(CoreEvent::StoriesChanged);
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
    for conversation_id in db.presage_forget_threads().await.map_err(|e| e.to_string())? {
        let done = match crate::send::thread(&conversation_id) {
            Some(thread) => store.clear_thread(&thread).await.map_err(|e| e.to_string()),
            // can never be cleared: dropped, or every sweep would fail on it
            None => Ok(()),
        };
        let done = match done {
            Ok(()) => db
                .presage_thread_forgotten(&conversation_id)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        if let Err(e) = done {
            first_error.get_or_insert(format!("deleting a stored chat: {e}"));
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
    // a pass fetching chats' pictures is under way
    let fetching_avatars = std::rc::Rc::new(std::cell::Cell::new(false));
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
                    let pictures = matches!(received, Received::QueueEmpty | Received::Contacts);
                    handle(received, &m, &db, &me, &mut progress, l.as_ref(), &queues).await;
                    caught_up.send_if_modified(|ready| {
                        let first = !*ready && progress.titles_synced;
                        *ready |= progress.titles_synced;
                        first
                    });
                    // chats' pictures, beside the receiving (never holding it up), one pass at a time
                    if pictures && !fetching_avatars.replace(true) {
                        let (m, db, me, l, running) =
                            (m.clone(), db.clone(), me.clone(), l.clone(), fetching_avatars.clone());
                        tokio::task::spawn_local(async move {
                            fetch_avatars(&m, &db, &me, l.as_ref()).await;
                            running.set(false);
                        });
                    }
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
    /// Groups fetched again for messages claiming a newer revision (`sync_group`).
    refetches: Refetches,
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
    // Kinds, counts and ids only: never message content.
    match &received {
        Received::QueueEmpty => tracing::info!("caught up with the server queue"),
        Received::Contacts => tracing::info!("contacts synced from the phone"),
        Received::DecryptionError { sender, error } => {
            tracing::warn!(sender = %sender.service_id_string(), %error, "a message couldn't be decrypted")
        }
        Received::RetryReceipt { sender, .. } => {
            tracing::info!(sender = %sender.service_id_string(), "a message of ours couldn't be decrypted there")
        }
        Received::Content(c) => tracing::info!(kind = content_kind(&c.body), "received"),
    }
    match received {
        Received::QueueEmpty => {
            if progress.drained() {
                sync_titles(m, db, l).await;
            }
            l.on_event(CoreEvent::Ready);
            let _ = queues.storage.send(storage::Trigger::Connected);
        }
        Received::Contacts => sync_titles(m, db, l).await,
        Received::DecryptionError { sender, error } => decryption_failed(db, l, sender, &error).await,
        // their client asks for a resend, which Hush doesn't do yet: nothing failed here
        Received::RetryReceipt { .. } => {}
        Received::Content(content) => {
            sync_group(m, db, l, &mut progress.refetches, &content).await;
            match drop_blocked(db, &content, me).await {
                Ok(true) => {
                    tracing::info!(kind = content_kind(&content.body), "dropped: blocked or not a member");
                    return;
                }
                Ok(false) => {}
                // shown rather than dropped: losing a message is worse than showing a blocked one
                Err(e) => l.on_event(CoreEvent::Error {
                    message: format!("checking whether a sender is blocked failed: {e}"),
                }),
            }
            if let presage::libsignal_service::content::ContentBody::CallMessage(cm) = &content.body {
                let _ = queues.calls.send(crate::calls::IncomingEnvelope {
                    sender: content.metadata.sender,
                    sender_device: content.metadata.sender_device.into(),
                    server_timestamp_ms: content.metadata.server_timestamp.timestamp_millis(),
                    message: cm.clone(),
                });
                return;
            }
            if storage::is_storage_fetch_latest(&content.body) {
                let _ = queues.storage.send(storage::Trigger::Phone);
            } else if storage::is_keys_sync(&content.body) {
                // presage stores the new master key as it receives it; a new key starts over
                let _ = queues.storage.send(storage::Trigger::Connected);
            }
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

/// Whether content comes from a blocked person, or belongs to a blocked group or to a group
/// the sender isn't a member of (messages, edits, typing, stories), so nothing should see it.
/// A dropped message's copy in presage's store (saved before we see it) is queued for the
/// sweep to delete. Our own devices are never blocked.
async fn drop_blocked(db: &ProjectionDb, content: &Content, me: &str) -> Result<bool, sqlx::Error> {
    use presage::libsignal_service::content::ContentBody;
    let sender = &content.metadata.sender;
    if sender.raw_uuid().to_string() == me {
        return Ok(false);
    }
    let group = match &content.body {
        ContentBody::DataMessage(dm) => group_chat(dm.group_v2.as_ref()),
        ContentBody::EditMessage(em) => group_chat(em.data_message.as_ref().and_then(|dm| dm.group_v2.as_ref())),
        ContentBody::TypingMessage(tm) => match &tm.group_id {
            Some(identifier) => db.group_by_identifier(identifier).await?,
            None => None,
        },
        ContentBody::StoryMessage(sm) => group_chat(sm.group.as_ref()),
        _ => None,
    };
    let blocked = db.is_blocked(&direct_id(sender)).await?
        || match &group {
            Some(g) => db.is_blocked(g).await? || outsider(db, g, sender).await?,
            None => false,
        };
    if blocked && matches!(content.body, ContentBody::DataMessage(_) | ContentBody::EditMessage(_)) {
        // still dropped if this fails: the copy only stays in presage's store
        let queued = match Thread::try_from(content) {
            Ok(thread) => {
                let at = content.metadata.client_timestamp.timestamp_millis();
                db.queue_presage_forget(&crate::projection::conversation_id(&thread), at)
                    .await
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = queued {
            tracing::warn!(error = %e, "a dropped message's stored copy wasn't queued for deletion");
        }
    }
    Ok(blocked)
}

/// Whether `sender` isn't among a group's members (a removed member still holding the key,
/// say), as last stored (`sync_group`): the group's latest members, as Signal's apps judge, so a
/// message delayed past its sender's removal goes too, and so does the change telling of
/// someone leaving (nothing shows for it anyway). A group never stored says nothing: its
/// members aren't known. The revision a message claims plays no part: anyone can claim one.
async fn outsider(db: &ProjectionDb, group: &str, sender: &ServiceId) -> Result<bool, sqlx::Error> {
    Ok(db.group_revision(group).await?.is_some()
        && !db.group_members(group).await?.contains(&sender.raw_uuid().to_string()))
}

/// The chat (`g:<hex>`) of a message's group context.
fn group_chat(g: Option<&presage::proto::GroupContextV2>) -> Option<String> {
    g.and_then(|g| g.master_key.as_deref()).map(hex_group)
}

/// `g:<hex>` for a master key as messages carry it.
fn hex_group(key: &[u8]) -> String {
    format!("g:{}", hex::encode(key))
}

/// Where the receive loop hands work on to.
#[derive(Clone)]
struct Queues {
    sends: mpsc::UnboundedSender<Job>,
    downloads: mpsc::UnboundedSender<Download>,
    calls: mpsc::UnboundedSender<crate::calls::IncomingEnvelope>,
    storage: mpsc::UnboundedSender<storage::Trigger>,
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

/// A message from `sender` that couldn't be decrypted: shown in the chat (not as an error
/// banner), with a way to reset the session. A duplicate isn't one: its first copy was
/// decrypted, then the server delivered it again (the run stopped before acknowledging it).
async fn decryption_failed(db: &ProjectionDb, l: &dyn EventListener, sender: ServiceId, error: &SignalProtocolError) {
    if matches!(error, SignalProtocolError::DuplicatedMessage(..)) {
        return;
    }
    let conversation_id = direct_id(&sender);
    let at = chrono::Utc::now().timestamp_millis();
    let author = sender.raw_uuid().to_string();
    match db
        .add_chat_event(&conversation_id, at, ChatEventKind::DecryptionFailed, &author, None)
        .await
    {
        Ok(()) => l.on_event(CoreEvent::ChatEventAdded { conversation_id }),
        Err(e) => l.on_event(CoreEvent::Error {
            message: format!("recording an undecryptable message failed: {e}"),
        }),
    }
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
/// Fetches the pictures of the listed chats not looked at in the last day (a few per pass): a
/// contact's profile avatar (with their profile key, ours for Note to Self), a group's avatar.
/// presage caches both without re-checking: a contact's new picture shows once their profile
/// key changes, a group's not at all (its cache is keyed by the group alone).
/// Runs beside the receiving, never holding it up.
async fn fetch_avatars(m: &Manager<SqliteStore, Registered>, db: &ProjectionDb, me: &str, l: &dyn EventListener) {
    use presage::libsignal_service::prelude::ProfileKey;
    let now = chrono::Utc::now().timestamp_millis();
    let ids = match db.avatars_to_fetch(now, 20).await {
        Ok(ids) => ids,
        Err(e) => return tracing::warn!(error = %e, "couldn't list the chats to fetch pictures for"),
    };
    let mut m = m.clone();
    let mut changed = false;
    for id in ids {
        let fetched: Result<Option<Vec<u8>>, String> = if let Some(aci) = id.strip_prefix("c:") {
            let key = match presage::libsignal_service::prelude::Uuid::parse_str(aci) {
                Ok(uuid) if aci == me => Some((uuid, m.registration_data().profile_key())),
                Ok(uuid) => match m.store().contact_by_id(&Aci::from(uuid).into()).await {
                    Ok(Some(c)) => <[u8; 32]>::try_from(c.profile_key.as_slice())
                        .ok()
                        .map(|k| (uuid, ProfileKey::create(k))),
                    _ => None,
                },
                // a phone-number-only chat: no profile to ask
                Err(_) => None,
            };
            match key {
                Some((uuid, key)) => timed_avatar(Box::pin(m.retrieve_profile_avatar_by_uuid(uuid, key))).await,
                None => Ok(None),
            }
        } else if let Some(hex_key) = id.strip_prefix("g:") {
            match hex::decode(hex_key) {
                Ok(master_key) => {
                    let context = presage::proto::GroupContextV2 {
                        master_key: Some(master_key),
                        revision: Some(0),
                        ..Default::default()
                    };
                    timed_avatar(Box::pin(m.retrieve_group_avatar(context))).await
                }
                Err(e) => Err(e.to_string()),
            }
        } else {
            Ok(None)
        };
        let stored = match fetched {
            Ok(bytes) => db.store_avatar(&id, bytes.as_deref(), now).await,
            Err(e) => {
                // ids only; tried again in a day
                tracing::warn!(conversation = %id, error = %e, "a chat's picture couldn't be fetched");
                db.set_avatar_checked(&id, now).await.map(|_| false)
            }
        };
        match stored {
            Ok(c) => changed |= c,
            Err(e) => tracing::warn!(conversation = %id, error = %e, "a chat's picture couldn't be kept"),
        }
    }
    if changed {
        l.on_event(CoreEvent::ConversationsChanged);
    }
}

/// A picture fetch, given 10 s.
async fn timed_avatar<E: std::fmt::Display>(
    fetch: impl std::future::Future<Output = Result<Option<Vec<u8>>, E>>,
) -> Result<Option<Vec<u8>>, String> {
    match tokio::time::timeout(Duration::from_secs(10), fetch).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

/// Stores a group's title, members, timer and where we (`own`) stand, as presage has
/// them, unless that revision is stored already; returns whether it was stored.
async fn store_group(
    db: &ProjectionDb,
    key: &[u8; 32],
    g: &presage::model::groups::Group,
    own: &ServiceIds,
) -> Result<bool, sqlx::Error> {
    let members: Vec<String> = g
        .members
        .iter()
        .map(|m| ServiceId::from(m.aci).raw_uuid().to_string())
        .collect();
    let timer = g.disappearing_messages_timer.as_ref().map_or(0, |t| t.duration);
    let standing = crate::groups::standing(g, own.aci, own.pni);
    db.set_group(&group_id(key), g.revision, &g.title, &members, timer, standing)
        .await
}

/// Lists the chat of a group that invites us (`own`), from now, unless the group or whoever
/// invited us is blocked; returns whether it was listed just now. Its state decides, not
/// whether this revision was stored just now, so whatever stored it first, it shows.
async fn list_invitation(
    db: &ProjectionDb,
    key: &[u8; 32],
    g: &presage::model::groups::Group,
    own: &ServiceIds,
) -> Result<bool, sqlx::Error> {
    let Some(invitation) = crate::groups::invitation(g, own.aci, own.pni) else {
        return Ok(false);
    };
    let id = group_id(key);
    if db.is_blocked(&id).await? || db.is_blocked(&direct_id(&invitation.added_by_aci.into())).await? {
        return Ok(false);
    }
    db.list_chat(&id, chrono::Utc::now().timestamp_millis()).await
}

/// Brings a group message's chat up to presage's group, so its sender is checked against
/// current members. presage refreshes the group before handing us a message sent at a newer
/// revision; if that refresh failed, a group presage knows is fetched once more, though not
/// for a blocked sender or group and at most every five minutes (`Refetches`): the revision
/// is whatever the sender wrote.
async fn sync_group(
    m: &Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    l: &dyn EventListener,
    refetches: &mut Refetches,
    content: &Content,
) {
    let Ok(Thread::Group(key)) = Thread::try_from(content) else {
        return;
    };
    let sent_at = sent_revision(&content.body);
    let group = match m.store().group(key).await {
        Ok(Some(g)) if sent_at.is_none_or(|r| r <= g.revision) => Ok(g),
        Ok(Some(g)) => {
            let blocked = db
                .is_blocked(&direct_id(&content.metadata.sender))
                .await
                .unwrap_or(true)
                || db.is_blocked(&group_id(&key)).await.unwrap_or(true);
            if blocked || !refetches.allow(key, std::time::Instant::now()) {
                Ok(g)
            } else {
                match tokio::time::timeout(Duration::from_secs(10), m.clone().fetch_group(&key)).await {
                    Ok(fetched) => fetched.map_err(|e| e.to_string()),
                    Err(_) => Err("fetching it timed out".into()),
                }
            }
        }
        // presage couldn't fetch it as the message came: not ours to try again
        Ok(None) => return,
        Err(e) => Err(e.to_string()),
    };
    let own = &m.registration_data().service_ids;
    let stored = match group {
        Ok(g) => match store_group(db, &key, &g, own).await {
            Ok(stored) => list_invitation(db, &key, &g, own).await.map(|listed| stored || listed),
            Err(e) => Err(e),
        }
        .map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    match stored {
        Ok(true) => l.on_event(CoreEvent::ConversationsChanged),
        Ok(false) => {}
        Err(e) => l.on_event(CoreEvent::Error {
            message: format!("syncing a group failed: {e}"),
        }),
    }
}

/// When each group was last fetched again by `sync_group`.
#[derive(Default)]
struct Refetches(std::collections::HashMap<[u8; 32], std::time::Instant>);

impl Refetches {
    const EVERY: Duration = Duration::from_secs(5 * 60);

    /// Whether the group may be fetched again at `now`; if so, counts it as fetched.
    fn allow(&mut self, key: [u8; 32], now: std::time::Instant) -> bool {
        if self
            .0
            .get(&key)
            .is_some_and(|last| now.duration_since(*last) < Self::EVERY)
        {
            return false;
        }
        self.0.insert(key, now);
        true
    }
}

/// The revision of the group a group message says it was sent at.
fn sent_revision(body: &presage::libsignal_service::content::ContentBody) -> Option<u32> {
    use presage::libsignal_service::content::ContentBody;
    let dm = match body {
        ContentBody::DataMessage(dm) => Some(dm),
        ContentBody::EditMessage(em) => em.data_message.as_ref(),
        ContentBody::SynchronizeMessage(sm) => match &sm.content {
            Some(presage::proto::sync_message::Content::Sent(sent)) => sent.message.as_ref(),
            _ => None,
        },
        _ => None,
    };
    dm.and_then(|dm| dm.group_v2.as_ref()).and_then(|g| g.revision)
}

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
                        let own = &m.registration_data().service_ids;
                        let stored = match store_group(db, &key, &g, own).await {
                            Ok(_) => list_invitation(db, &key, &g, own).await,
                            Err(e) => Err(e),
                        };
                        if let Err(e) = stored {
                            problems.push(format!("storing a group: {e}"));
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

    use super::{race_link, Command, Raced};
    use crate::events::{CoreEvent, EventListener};
    use crate::projection::{Op, ProjectionDb, Settings};
    use tokio::sync::{mpsc, oneshot};

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
                ranges: vec![],
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

    /// A message delivered again (its first copy was decrypted, then the run stopped before the
    /// server heard so) is dropped like Signal's clients do: no event in the chat, no reset.
    #[tokio::test]
    async fn only_a_real_decryption_failure_is_shown() {
        use presage::libsignal_service::protocol::{Aci, CiphertextMessageType, SignalProtocolError};
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let sink = Sink(Default::default());
        let alice = Aci::from_uuid_bytes([1; 16]);
        let chat = crate::projection::ids::direct_id(&alice.into());
        super::decryption_failed(
            &db,
            &sink,
            alice.into(),
            &SignalProtocolError::DuplicatedMessage(61, 58),
        )
        .await;
        assert!(db.chat_events(&chat, 0).await.unwrap().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
        let broken = SignalProtocolError::InvalidMessage(CiphertextMessageType::Whisper, "decryption failed".into());
        super::decryption_failed(&db, &sink, alice.into(), &broken).await;
        assert_eq!(db.chat_events(&chat, 0).await.unwrap().len(), 1);
        assert!(matches!(
            &sink.0.lock().unwrap()[..],
            [CoreEvent::ChatEventAdded { .. }]
        ));
    }

    /// The receipts flush is queued after this returns, so the new setting must already be stored.
    #[tokio::test]
    async fn a_settings_change_is_stored_before_it_is_reported() {
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let off = Op::Settings {
            read_receipts: Some(false),
            typing_indicators: None,
            link_previews: None,
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
                typing_indicators: None,
                link_previews: None,
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
            ranges: vec![],
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

    fn content_from(
        sender: &str,
        ms: u64,
        body: presage::libsignal_service::content::ContentBody,
    ) -> presage::libsignal_service::content::Content {
        use presage::libsignal_service::content::{Content, Metadata};
        use presage::libsignal_service::protocol::{Aci, DeviceId, ServiceId};
        let sid = ServiceId::from(Aci::from(
            presage::libsignal_service::prelude::Uuid::parse_str(sender).unwrap(),
        ));
        let at = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap();
        Content {
            metadata: Metadata {
                sender: sid,
                destination: sid,
                sender_device: DeviceId::new(1).unwrap(),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    /// Whatever a blocked person sends, and what anyone sends in a blocked group, is dropped
    /// before it reaches the chat, the call UI or a notification; presage's copy is queued to go.
    #[tokio::test]
    async fn what_blocked_senders_and_groups_send_is_dropped() {
        use crate::projection::ProjectionDb;
        use presage::libsignal_service::content::ContentBody;
        use presage::proto::{typing_message, CallMessage, DataMessage, GroupContextV2, SyncMessage, TypingMessage};
        const ME: &str = "11111111-1111-1111-1111-111111111111";
        const ALICE: &str = "22222222-2222-2222-2222-222222222222";
        const BOB: &str = "33333333-3333-3333-3333-333333333333";
        let key = [7u8; 32];
        let group = crate::projection::ids::group_id(&key);
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let text = |ms: u64, group: Option<Vec<u8>>| {
            ContentBody::DataMessage(DataMessage {
                body: Some("x".into()),
                timestamp: Some(ms),
                group_v2: group.map(|k| GroupContextV2 {
                    master_key: Some(k),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let typing = |group_id: Option<Vec<u8>>| {
            ContentBody::TypingMessage(TypingMessage {
                timestamp: Some(1),
                action: Some(typing_message::Action::Started as i32),
                group_id,
            })
        };
        for id in [format!("c:{ALICE}"), group.clone()] {
            db.set_phone_contact(&id, false).await.unwrap(); // makes the chat's row
            db.block(&id).await.unwrap();
        }
        let dropped = |c| {
            let db = &db;
            async move { super::drop_blocked(db, &c, ME).await.unwrap() }
        };
        assert!(dropped(content_from(ALICE, 100, text(100, None))).await);
        assert!(dropped(content_from(ALICE, 2, typing(None))).await);
        assert!(dropped(content_from(ALICE, 3, ContentBody::CallMessage(CallMessage::default()))).await);
        assert!(dropped(content_from(BOB, 200, text(200, Some(key.to_vec())))).await);
        let edit = |ms: u64, group: Option<Vec<u8>>| match text(ms, group) {
            ContentBody::DataMessage(dm) => ContentBody::EditMessage(presage::proto::EditMessage {
                target_sent_timestamp: Some(1),
                data_message: Some(dm),
            }),
            _ => unreachable!(),
        };
        assert!(dropped(content_from(ALICE, 101, edit(101, None))).await);
        assert!(dropped(content_from(BOB, 201, edit(201, Some(key.to_vec())))).await);
        let identifier = crate::projection::ids::group_identifier(&group);
        assert!(dropped(content_from(BOB, 4, typing(identifier))).await);
        // a story posted in a blocked group
        let story = ContentBody::StoryMessage(presage::proto::StoryMessage {
            group: Some(GroupContextV2 {
                master_key: Some(key.to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(dropped(content_from(BOB, 6, story)).await);
        assert!(
            !dropped(content_from(BOB, 300, text(300, None))).await,
            "bob alone isn't blocked"
        );
        assert!(
            !dropped(content_from(
                ME,
                5,
                ContentBody::SynchronizeMessage(SyncMessage::default())
            ))
            .await,
            "our own devices are never blocked"
        );
        let mut queued = db.presage_forget_queue().await.unwrap();
        queued.sort();
        assert_eq!(
            queued,
            vec![
                (format!("c:{ALICE}"), 100),
                (format!("c:{ALICE}"), 101),
                (group.clone(), 200),
                (group, 201)
            ],
            "presage's copies of the dropped messages go too"
        );
    }

    /// In a group whose members are known, whatever someone outside it sends (a removed member,
    /// or anyone else holding the key) is dropped like a blocked sender's; members and our own
    /// devices get through, and a group whose members aren't known drops nothing.
    #[tokio::test]
    async fn what_non_members_send_in_a_group_is_dropped() {
        use crate::projection::ProjectionDb;
        use presage::libsignal_service::content::ContentBody;
        use presage::proto::{typing_message, DataMessage, GroupContextV2, TypingMessage};
        const ME: &str = "11111111-1111-1111-1111-111111111111";
        const ALICE: &str = "22222222-2222-2222-2222-222222222222";
        const MALLORY: &str = "44444444-4444-4444-4444-444444444444";
        let (known, unknown) = ([7u8; 32], [8u8; 32]);
        let group = crate::projection::ids::group_id(&known);
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        db.set_group(
            &group,
            1,
            "G",
            &[ME.into(), ALICE.into()],
            0,
            super::GroupStanding::Other,
        )
        .await
        .unwrap();
        let text = |ms: u64, key: [u8; 32]| {
            ContentBody::DataMessage(DataMessage {
                body: Some("x".into()),
                timestamp: Some(ms),
                group_v2: Some(GroupContextV2 {
                    master_key: Some(key.to_vec()),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let dropped = |c| {
            let db = &db;
            async move { super::drop_blocked(db, &c, ME).await.unwrap() }
        };
        assert!(dropped(content_from(MALLORY, 100, text(100, known))).await);
        let edit = ContentBody::EditMessage(presage::proto::EditMessage {
            target_sent_timestamp: Some(100),
            data_message: match text(101, known) {
                ContentBody::DataMessage(dm) => Some(dm),
                _ => unreachable!(),
            },
        });
        assert!(dropped(content_from(MALLORY, 101, edit)).await);
        let typing = ContentBody::TypingMessage(TypingMessage {
            timestamp: Some(1),
            action: Some(typing_message::Action::Started as i32),
            group_id: crate::projection::ids::group_identifier(&group),
        });
        assert!(dropped(content_from(MALLORY, 2, typing)).await);
        let story = ContentBody::StoryMessage(presage::proto::StoryMessage {
            group: Some(GroupContextV2 {
                master_key: Some(known.to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(dropped(content_from(MALLORY, 3, story)).await);
        assert!(!dropped(content_from(ALICE, 200, text(200, known))).await, "a member");
        assert!(
            !dropped(content_from(ME, 300, text(300, known))).await,
            "our own devices"
        );
        assert!(
            !dropped(content_from(MALLORY, 400, text(400, unknown))).await,
            "members not known"
        );
        let mut newer = text(500, known);
        if let ContentBody::DataMessage(dm) = &mut newer {
            dm.group_v2.as_mut().unwrap().revision = Some(2);
        }
        assert!(
            dropped(content_from(MALLORY, 500, newer)).await,
            "a revision the sender claims doesn't turn the check off"
        );
        assert_eq!(
            db.presage_forget_queue().await.unwrap(),
            vec![(group.clone(), 100), (group.clone(), 101), (group, 500)],
            "presage's copies of the dropped messages go too"
        );
    }

    /// A group is fetched again at most every five minutes, whatever its messages claim.
    #[test]
    fn a_group_is_refetched_at_most_every_five_minutes() {
        use std::time::{Duration, Instant};
        let mut refetches = super::Refetches::default();
        let t = Instant::now();
        assert!(refetches.allow([1; 32], t));
        assert!(!refetches.allow([1; 32], t + Duration::from_secs(60)));
        assert!(refetches.allow([2; 32], t + Duration::from_secs(60)), "another group");
        assert!(refetches.allow([1; 32], t + Duration::from_secs(301)));
    }

    const ALICE: &str = "22222222-2222-2222-2222-222222222222";

    /// Our ids in tests: an ACI and a PNI that no test group knows.
    fn own() -> super::ServiceIds {
        use presage::libsignal_service::prelude::Uuid;
        super::ServiceIds {
            aci: Uuid::from_u128(0xa),
            pni: Uuid::from_u128(0xb),
        }
    }

    /// "Hikers" at `revision`: Alice (an admin), a minute's timer.
    fn hikers(revision: u32) -> presage::model::groups::Group {
        use presage::libsignal_service::groups_v2::{Role, Timer};
        use presage::libsignal_service::prelude::{ProfileKey, Uuid};
        use presage::libsignal_service::protocol::Aci;
        use presage::model::groups::{Group, Member};
        Group {
            title: "Hikers".into(),
            avatar: String::new(),
            disappearing_messages_timer: Some(Timer { duration: 60 }),
            access_control: None,
            revision,
            members: vec![Member {
                aci: Aci::from(Uuid::parse_str(ALICE).unwrap()),
                role: Role::Administrator,
                profile_key: ProfileKey::create([0; 32]),
                joined_at_revision: 0,
                label: None,
                label_emoji: None,
            }],
            pending_members: vec![],
            requesting_members: vec![],
            invite_link_password: vec![],
            description: None,
        }
    }

    /// A group that invites our number (or our account) is stored as an invitation; once
    /// we're in, it isn't.
    #[tokio::test]
    async fn a_group_inviting_us_is_stored_as_an_invitation() {
        use presage::libsignal_service::groups_v2::Role;
        use presage::model::groups::PendingMember;
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap();
        let key = [4u8; 32];
        let id = crate::projection::ids::group_id(&key);
        let mut g = hikers(1);
        g.pending_members = vec![PendingMember {
            uuid: own().pni,
            service_id_type: presage::model::ServiceIdType::PhoneNumberIdentity,
            role: Role::Default,
            added_by_aci: g.members[0].aci,
            timestamp: 0,
        }];
        assert!(super::store_group(&db, &key, &g, &own()).await.unwrap());
        let inviter = super::direct_id(&g.members[0].aci.into());
        db.upsert_conversation_title(&inviter, "Alice").await.unwrap();
        db.block(&inviter).await.unwrap();
        assert!(
            !super::list_invitation(&db, &key, &g, &own()).await.unwrap(),
            "a blocked inviter's invitation stays out"
        );
        db.accept(&inviter).await.unwrap();
        db.block(&id).await.unwrap();
        assert!(
            !super::list_invitation(&db, &key, &g, &own()).await.unwrap(),
            "so does a blocked group's"
        );
        db.accept(&id).await.unwrap();
        assert!(
            super::list_invitation(&db, &key, &g, &own()).await.unwrap(),
            "listed though its revision was stored before"
        );
        let invitation = |chats: Vec<crate::projection::model::Conversation>| {
            chats.iter().find(|c| c.id == id).map(|c| c.group_invitation)
        };
        assert_eq!(invitation(db.conversations().await.unwrap()), Some(true));
        g.pending_members.clear();
        g.revision = 2;
        assert!(super::store_group(&db, &key, &g, &own()).await.unwrap());
        assert_eq!(invitation(db.conversations().await.unwrap()), Some(false));
    }

    /// presage's group lands in the chat: its title, members' ACIs and timer, at its revision.
    #[tokio::test]
    async fn presages_group_is_stored_in_the_chat() {
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap();
        let key = [7u8; 32];
        assert!(super::store_group(&db, &key, &hikers(3), &own()).await.unwrap());
        let id = crate::projection::ids::group_id(&key);
        assert_eq!(db.group_members(&id).await.unwrap(), vec![ALICE]);
        assert_eq!(db.title(&id).await.unwrap().as_deref(), Some("Hikers"));
        assert_eq!(db.expire_timer(&id).await.unwrap(), 60);
        assert_eq!(db.group_revision(&id).await.unwrap(), Some(3));
    }

    /// After a change (whatever presage answered: it may have failed after the server took it),
    /// the chat takes presage's group, and a picture we just set or removed.
    #[tokio::test]
    async fn after_a_group_change_the_chat_takes_the_group_and_its_new_picture() {
        use presage::model::identity::OnNewIdentity;
        use presage::store::ContentsStore;
        use presage_store_sqlite::SqliteStore;
        let dir = std::env::temp_dir().join(format!("sig-core-settle-group-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteStore::open_with_options(
            super::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap()
            .with_files(dir.join("attachments"));
        let key = [7u8; 32];
        let id = crate::projection::ids::group_id(&key);
        store.save_group(key, hikers(4)).await.unwrap();
        let sink = Sink(std::sync::Mutex::new(vec![]));
        super::settle_group(&store, &own(), &db, &sink, &key, Some(b"jpeg")).await;
        assert_eq!(db.group_revision(&id).await.unwrap(), Some(4));
        assert!(
            !db.store_avatar(&id, Some(b"jpeg"), 1).await.unwrap(),
            "the chat has the picture already"
        );
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [CoreEvent::ConversationsChanged]
        ));
        super::settle_group(&store, &own(), &db, &sink, &key, Some(b"")).await;
        assert!(!db.store_avatar(&id, None, 2).await.unwrap(), "removed already");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A group we just made is listed at once under its name, though it has no messages.
    #[tokio::test]
    async fn a_new_group_is_listed_at_once() {
        use presage::libsignal_service::prelude::Uuid;
        use presage::libsignal_service::protocol::Aci;
        use presage::model::identity::OnNewIdentity;
        use presage::store::ContentsStore;
        use presage_store_sqlite::SqliteStore;
        let dir = std::env::temp_dir().join(format!("sig-core-new-group-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteStore::open_with_options(
            super::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap();
        let key = [9u8; 32];
        let before = super::group_keys(&store).await.unwrap();
        let alice = Aci::from(Uuid::parse_str(ALICE).unwrap());
        let bob = Aci::from(Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap());
        store.save_group([8; 32], hikers(3)).await.unwrap();
        assert_eq!(
            super::our_new_group(&store, &before, &alice, "Hikers", &[])
                .await
                .unwrap(),
            None,
            "past its first revision"
        );
        let before = super::group_keys(&store).await.unwrap();
        // Hikers: Alice its only member and admin
        store.save_group(key, hikers(0)).await.unwrap();
        assert_eq!(
            super::our_new_group(&store, &before, &bob, "Hikers", &[alice])
                .await
                .unwrap(),
            None,
            "Alice made it and added Bob: not his, though he asked for her and the same name"
        );
        assert_eq!(
            super::our_new_group(&store, &before, &alice, "Climbers", &[])
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            super::our_new_group(&store, &before, &alice, "Hikers", &[])
                .await
                .unwrap(),
            Some(key)
        );
        let sink = Sink(std::sync::Mutex::new(vec![]));
        super::list_new_group(&store, &own(), &db, &sink, &[6; 32], 1234).await;
        assert!(
            matches!(
                sink.0.lock().unwrap().as_slice(),
                [CoreEvent::Error { .. }, CoreEvent::ConversationsChanged]
            ),
            "a group presage hasn't is said, and the list refreshed all the same"
        );
        sink.0.lock().unwrap().clear();
        super::list_new_group(&store, &own(), &db, &sink, &key, 1234).await;
        let chats = db.conversations().await.unwrap();
        let chat = chats
            .iter()
            .find(|c| c.id == crate::projection::ids::group_id(&key))
            .expect("listed");
        assert_eq!((chat.title.as_str(), chat.last_message_at), ("Hikers", 1234));
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [CoreEvent::ConversationsChanged]
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only one new group is taken for the one we made: two could be one that arrived meanwhile.
    #[test]
    fn the_group_made_is_the_only_new_one() {
        let set = |keys: &[u8]| keys.iter().map(|k| [*k; 32]).collect::<std::collections::HashSet<_>>();
        assert_eq!(super::made_group(&set(&[1]), &set(&[1, 2])), Some([2; 32]));
        assert_eq!(super::made_group(&set(&[1]), &set(&[1])), None);
        assert_eq!(super::made_group(&set(&[1]), &set(&[1, 2, 3])), None);
    }

    /// A deleted chat's messages leave presage's store too.
    #[tokio::test]
    async fn a_deleted_chat_leaves_presages_store_too() {
        use presage::libsignal_service::content::ContentBody;
        use presage::libsignal_service::protocol::{Aci, ServiceId};
        use presage::model::identity::OnNewIdentity;
        use presage::proto::DataMessage;
        use presage::store::{ContentsStore, Thread};
        use presage_store_sqlite::SqliteStore;

        const ALICE: &str = "22222222-2222-2222-2222-222222222222";
        let dir = std::env::temp_dir().join(format!("sig-core-delete-chat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = SqliteStore::open_with_options(
            super::sqlite_options(&dir.join("presage.db"), &"00".repeat(32)).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        let alice = ServiceId::from(Aci::from(
            presage::libsignal_service::prelude::Uuid::parse_str(ALICE).unwrap(),
        ));
        let thread = Thread::Contact(alice);
        for ms in [100, 200] {
            let body = ContentBody::DataMessage(DataMessage {
                timestamp: Some(ms),
                ..Default::default()
            });
            store
                .save_message(&thread, content_from(ALICE, ms, body))
                .await
                .unwrap();
        }
        let db = crate::projection::ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap();
        db.set_phone_contact(&format!("c:{ALICE}"), false).await.unwrap();
        db.delete_chat(&format!("c:{ALICE}")).await.unwrap();
        super::sweep(&mut store, &db, &Quiet).await.unwrap();
        assert!(store.message(&thread, 100).await.unwrap().is_none());
        assert!(store.message(&thread, 200).await.unwrap().is_none());
        assert!(db.presage_forget_threads().await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A link that is never scanned, with the commands `send` puts in first.
    async fn race_pending(send: impl FnOnce(&mpsc::UnboundedSender<Command>)) -> (Raced<()>, Vec<Command>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        send(&tx);
        drop(tx);
        let mut queued = std::collections::VecDeque::new();
        let raced = race_link(Box::pin(futures::future::pending::<()>()), &mut rx, &mut queued).await;
        (raced, queued.into_iter().collect())
    }

    #[tokio::test]
    async fn a_code_waiting_for_a_scan_stops_for_a_restore_and_queues_the_rest() {
        let (done, stopped) = oneshot::channel();
        let (raced, queued) = race_pending(|tx| {
            tx.send(Command::Start).unwrap();
            tx.send(Command::StopUnlinked { done }).unwrap();
        })
        .await;
        assert_eq!(raced, Raced::Stopped);
        assert!(stopped.await.unwrap(), "stopped unlinked");
        assert!(matches!(queued[..], [Command::Start]));
    }

    #[tokio::test]
    async fn relinking_or_unlinking_stop_a_code_too() {
        let (done, relinked) = oneshot::channel();
        let (raced, _) = race_pending(|tx| tx.send(Command::Relink { done }).unwrap()).await;
        assert_eq!(raced, Raced::Stopped);
        relinked.await.unwrap();
        let (done, unlinked) = oneshot::channel();
        let (raced, _) = race_pending(|tx| {
            tx.send(Command::Unlink {
                tell_server: true,
                done,
            })
            .unwrap()
        })
        .await;
        assert_eq!(raced, Raced::Stopped);
        assert_eq!(unlinked.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_closed_client_ends_the_wait() {
        let (raced, _) = race_pending(|_| {}).await;
        assert_eq!(raced, Raced::Closed);
    }

    #[tokio::test]
    async fn a_finished_link_keeps_what_was_queued() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Command::Start).unwrap();
        let mut queued = std::collections::VecDeque::new();
        let link = Box::pin(async {
            tokio::task::yield_now().await;
            7
        });
        assert_eq!(race_link(link, &mut rx, &mut queued).await, Raced::Linked(7));
        // the queued Start is either taken in or still in the channel: never lost
        assert_eq!(queued.len() + usize::from(rx.try_recv().is_ok()), 1);
    }
}
