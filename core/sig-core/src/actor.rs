//! The core thread. presage's `Manager` and store are `!Send`, so everything that touches them runs
//! here: one OS thread with a current-thread tokio runtime and a `LocalSet`. The rest of the core
//! talks to it through `Command`s; results flow back as `CoreEvent`s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{channel::oneshot as fut_oneshot, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::ContentsStore;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::{mpsc, oneshot};

use crate::error::CoreError;
use crate::events::{from_applied, ConnectionState, CoreEvent, EventListener};
use crate::projection::ids::{direct_id, group_id};
use crate::projection::{project, ProjectionDb};

pub enum Command {
    Link { device_name: String },
    Start,
}

pub struct Handles {
    pub commands: mpsc::UnboundedSender<Command>,
    pub projection: ProjectionDb,
}

/// Starts the core thread, opens both databases and waits until they are ready. `linked` is kept
/// up to date by the thread.
pub fn spawn(
    data_dir: PathBuf,
    key: String,
    listener: Arc<dyn EventListener>,
    linked: Arc<AtomicBool>,
) -> Result<Handles, CoreError> {
    std::fs::create_dir_all(&data_dir).map_err(|e| CoreError::Storage { message: e.to_string() })?;
    let (commands, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("sig-core".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(CoreError::Storage { message: e.to_string() }));
                    return;
                }
            };
            tokio::task::LocalSet::new().block_on(&rt, run(data_dir, key, listener, linked, rx, ready_tx));
        })
        .map_err(|e| CoreError::Storage { message: e.to_string() })?;
    let projection = ready_rx.blocking_recv().map_err(|_| CoreError::Closed)??;
    Ok(Handles { commands, projection })
}

async fn run(
    dir: PathBuf,
    key: String,
    listener: Arc<dyn EventListener>,
    linked: Arc<AtomicBool>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    ready: oneshot::Sender<Result<ProjectionDb, CoreError>>,
) {
    let opened = async {
        let store_url = format!("sqlite://{}", dir.join("presage.db").display());
        let store = SqliteStore::open_with_passphrase(&store_url, Some(&key), OnNewIdentity::Trust)
            .await
            .map_err(|e| CoreError::Storage { message: e.to_string() })?;
        let projection =
            ProjectionDb::open(&format!("sqlite://{}", dir.join("projection.db").display()), Some(&key)).await?;
        Ok::<_, CoreError>((store, projection))
    };
    let (store, projection) = match opened.await {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut manager = Manager::load_registered(store.clone()).await.ok();
    linked.store(manager.is_some(), Ordering::SeqCst);
    if ready.send(Ok(projection.clone())).is_err() {
        return;
    }

    let mut receiving = false;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Link { device_name } => {
                if manager.is_some() {
                    listener.on_event(CoreEvent::Error {
                        message: "already linked".into(),
                    });
                    continue;
                }
                let (url_tx, url_rx) = fut_oneshot::channel();
                let l = listener.clone();
                let (result, _) = futures::future::join(
                    Manager::link_secondary_device(store.clone(), SignalServers::Production, device_name, url_tx),
                    async move {
                        if let Ok(url) = url_rx.await {
                            l.on_event(CoreEvent::LinkUrl { url: url.to_string() });
                        }
                    },
                )
                .await;
                match result {
                    Ok(mut m) => {
                        if let Err(e) = m.request_contacts().await {
                            tracing::warn!(error = %e, "requesting contacts after linking failed");
                        }
                        manager = Some(m);
                        linked.store(true, Ordering::SeqCst);
                        listener.on_event(CoreEvent::Linked);
                    }
                    Err(e) => {
                        listener.on_event(CoreEvent::Error {
                            message: format!("linking failed: {e}"),
                        });
                        listener.on_event(CoreEvent::Connection {
                            state: ConnectionState::Unlinked,
                        });
                    }
                }
            }
            Command::Start => match (&manager, receiving) {
                (Some(m), false) => {
                    receiving = true;
                    tokio::task::spawn_local(receive_loop(m.clone(), projection.clone(), listener.clone()));
                }
                (Some(_), true) => {}
                (None, _) => listener.on_event(CoreEvent::Connection {
                    state: ConnectionState::Unlinked,
                }),
            },
        }
    }
}

/// Receives forever, reconnecting with exponential backoff (1 s → 60 s).
async fn receive_loop(mut m: Manager<SqliteStore, Registered>, db: ProjectionDb, l: Arc<dyn EventListener>) {
    let me = m.registration_data().service_ids.aci.to_string();
    let mut backoff = Duration::from_secs(1);
    loop {
        l.on_event(CoreEvent::Connection {
            state: ConnectionState::Connecting,
        });
        match m.receive_messages().await {
            Ok(stream) => {
                backoff = Duration::from_secs(1);
                l.on_event(CoreEvent::Connection {
                    state: ConnectionState::Online,
                });
                futures::pin_mut!(stream);
                while let Some(received) = stream.next().await {
                    handle(received, &m, &db, &me, l.as_ref()).await;
                }
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

async fn handle(
    received: Received,
    m: &Manager<SqliteStore, Registered>,
    db: &ProjectionDb,
    me: &str,
    l: &dyn EventListener,
) {
    match received {
        Received::QueueEmpty => {
            sync_titles(m, db, l).await;
            l.on_event(CoreEvent::Ready);
        }
        Received::Contacts => sync_titles(m, db, l).await,
        // Ids only: never log message content.
        Received::DecryptionError(sender) => tracing::warn!(sender = %sender.service_id_string(), "decryption error"),
        Received::Content(content) => {
            for op in project(&content, me) {
                match db.apply(&op).await {
                    Ok(applied) => applied.into_iter().for_each(|a| l.on_event(from_applied(a))),
                    Err(e) => l.on_event(CoreEvent::Error {
                        message: format!("storing a message failed: {e}"),
                    }),
                }
            }
        }
    }
}

/// Copies contact names and group titles from presage's store into the projection.
async fn sync_titles(m: &Manager<SqliteStore, Registered>, db: &ProjectionDb, l: &dyn EventListener) {
    let mut failed = 0usize;
    match m.store().contacts().await {
        Ok(contacts) => {
            for c in contacts.flatten().filter(|c| !c.name.is_empty()) {
                let id = direct_id(&presage::libsignal_service::protocol::Aci::from(c.uuid).into());
                failed += usize::from(db.upsert_conversation_title(&id, &c.name).await.is_err());
            }
        }
        Err(_) => failed += 1,
    }
    match m.store().groups().await {
        Ok(groups) => {
            for (key, g) in groups.flatten() {
                failed += usize::from(db.upsert_conversation_title(&group_id(&key), &g.title).await.is_err());
            }
        }
        Err(_) => failed += 1,
    }
    if failed > 0 {
        l.on_event(CoreEvent::Error {
            message: format!("{failed} contact/group names could not be stored"),
        });
    }
    l.on_event(CoreEvent::ConversationsChanged);
}
