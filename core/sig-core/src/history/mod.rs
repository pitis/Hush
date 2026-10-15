//! Importing the phone's message history at link time (link-and-sync).

// The pieces land one by one and are wired together by the driver; until then they are unused.
// TODO(remove when the driver lands): drop this allow.
#![allow(dead_code)]

pub(crate) mod fetch;
pub(crate) mod import;
pub(crate) mod read;
#[cfg(test)]
pub(crate) mod test_archive;

/// What the app shows while history arrives.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStage {
    /// Waiting for the phone to upload the archive.
    Waiting,
    Downloading,
    Importing,
    /// The history is in; the chat list is fresh.
    Done,
    /// There is no history to import (declined, cancelled, or the phone gave up).
    NoHistory,
    /// The transfer failed; Retry and Continue Without History are the ways out.
    Failed,
}

use std::path::Path;

use presage::libsignal_service::prelude::Uuid;
use presage::store::ContentsStore as _;
use presage_store_sqlite::SqliteStore;
use tokio::sync::watch;

use crate::events::{CoreEvent, EventListener};
use crate::projection::{HistoryDbStage, ProjectionDb};

/// How a finished import ended (failures are the `Err` side).
#[derive(Debug)]
pub(crate) enum Outcome {
    Done,
    /// There is no archive to import (declined, the phone gave up, or it expired).
    NoHistory,
    Cancelled,
}

/// The whole import: wait, download, verify, read, import, seed presage, clean up. Emits
/// `HistoryProgress` along the way. When it returns `Ok`, the state row and temp file are gone;
/// on an error both stay for Retry, except that a bad archive also resets the download, so
/// Retry fetches it afresh.
pub(crate) async fn run(
    db: &ProjectionDb,
    store: &mut SqliteStore,
    server: &mut impl fetch::ArchiveServer,
    tmp: &Path,
    l: &dyn EventListener,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Outcome, HistoryError> {
    match fetch::fetch_archive(server, db, tmp, l, cancel).await? {
        fetch::Fetched::NoUpload => {
            cleanup(db, tmp).await?;
            l.on_event(CoreEvent::Error {
                message: "Your phone didn't send message history.".into(),
            });
            no_history(l);
            return Ok(Outcome::NoHistory);
        }
        fetch::Fetched::Cancelled => {
            cleanup(db, tmp).await?;
            no_history(l);
            return Ok(Outcome::Cancelled);
        }
        fetch::Fetched::File => {}
    }
    let state = db
        .history_state()
        .await?
        .ok_or_else(|| HistoryError::Server("the import state vanished".into()))?;
    let key: [u8; 32] = state
        .key
        .as_slice()
        .try_into()
        .map_err(|_| HistoryError::Archive("bad key".into()))?;
    let aci = Uuid::parse_str(&state.aci).map_err(|e| HistoryError::Archive(e.to_string()))?;
    l.on_event(CoreEvent::HistoryProgress {
        stage: HistoryStage::Importing,
        done: 0,
        total: 0,
    });
    let mut frames = read::stream(tmp.to_path_buf(), key, aci);
    let mut importer = import::Importer::new(&aci.to_string(), chrono::Utc::now().timestamp_millis());
    let mut items: u64 = 0;
    loop {
        if *cancel.borrow() {
            cleanup(db, tmp).await?;
            no_history(l);
            return Ok(Outcome::Cancelled);
        }
        let Some(next) = frames.recv().await else { break };
        let frame = match next {
            Ok(f) => f,
            Err(e) => {
                // nothing of a bad archive is kept, and Retry downloads it again
                let _ = std::fs::remove_file(tmp);
                let mut reset = state.clone();
                reset.stage = HistoryDbStage::Downloading;
                reset.bytes_done = 0;
                reset.total_bytes = None;
                db.set_history_state(&reset).await?;
                return Err(HistoryError::Archive(e));
            }
        };
        if let Some(batch) = importer.add(frame) {
            items += batch.len() as u64;
            db.import_history(&batch).await?;
            l.on_event(CoreEvent::HistoryProgress {
                stage: HistoryStage::Importing,
                done: items,
                total: 0,
            });
        }
    }
    let (rest, seed, counts) = importer.finish();
    items += rest.len() as u64;
    db.import_history(&rest).await?;
    // What presage needs to send: contacts with profile keys, group master keys. Failures are
    // logged, not fatal: the phone's contact sync fills the same gap.
    for contact in &seed.contacts {
        if let Err(e) = store.save_contact(contact).await {
            tracing::warn!("seeding a contact failed: {e}");
        }
    }
    for (master_key, group) in seed.groups {
        if let Err(e) = store.save_group(master_key, group).await {
            tracing::warn!("seeding a group failed: {e}");
        }
    }
    tracing::info!(
        messages = counts.messages,
        conversations = counts.conversations,
        skipped = counts.skipped,
        expired = counts.expired,
        unavailable_files = counts.unavailable_files,
        "history imported"
    );
    cleanup(db, tmp).await?;
    l.on_event(CoreEvent::HistoryProgress {
        stage: HistoryStage::Done,
        done: items,
        total: 0,
    });
    l.on_event(CoreEvent::ConversationsChanged);
    Ok(Outcome::Done)
}

async fn cleanup(db: &ProjectionDb, tmp: &Path) -> Result<(), sqlx::Error> {
    let _ = std::fs::remove_file(tmp);
    db.clear_history_state().await
}

fn no_history(l: &dyn EventListener) {
    l.on_event(CoreEvent::HistoryProgress {
        stage: HistoryStage::NoHistory,
        done: 0,
        total: 0,
    });
}

/// Why a history import stopped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryError {
    /// The chat server refused or couldn't be reached.
    #[error("server: {0}")]
    Server(String),
    /// The archive download failed.
    #[error("download: {0}")]
    Download(String),
    /// The archive couldn't be verified or read (HMAC, decryption, gzip, protobuf).
    #[error("the archive couldn't be read: {0}")]
    Archive(String),
    #[error("storage: {0}")]
    Db(#[from] sqlx::Error),
}

/// The provisioning URL with `capabilities=backup5` appended, which makes the phone offer
/// "Transfer message history" when it scans the QR code.
pub(crate) fn with_backup_capability(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&capabilities=backup5")
    } else {
        format!("{url}?capabilities=backup5")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_qr_url_asks_for_history() {
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice?uuid=u&pub_key=k"),
            "sgnl://linkdevice?uuid=u&pub_key=k&capabilities=backup5"
        );
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice"),
            "sgnl://linkdevice?capabilities=backup5"
        );
    }
}

#[cfg(test)]
mod driver_tests {
    use fetch::test_support::{Fake, Quiet};
    use fetch::Polled;
    use libsignal_message_backup::proto::backup;
    use presage::model::identity::OnNewIdentity;
    use protobuf::MessageField;

    use super::*;
    use crate::projection::HistoryImportState;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ALICE: &str = "22222222-2222-2222-2222-222222222222";
    const KEY: [u8; 32] = [9; 32];

    struct Sink(std::sync::Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }
    impl Sink {
        fn stages(&self) -> Vec<HistoryStage> {
            let mut out = vec![];
            for e in self.0.lock().unwrap().iter() {
                if let CoreEvent::HistoryProgress { stage, .. } = e {
                    if out.last() != Some(stage) {
                        out.push(*stage);
                    }
                }
            }
            out
        }
    }

    /// A temp dir removed on drop, even if the test panics.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("sig-run-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn frames() -> Vec<backup::Frame> {
        let frame = |item| backup::Frame {
            item: Some(item),
            ..Default::default()
        };
        let text = |body: &str| {
            MessageField::some(backup::Text {
                body: body.into(),
                ..Default::default()
            })
        };
        let item = |sent_at: u64, body: &str| {
            frame(backup::frame::Item::ChatItem(backup::ChatItem {
                chatId: 10,
                authorId: 1,
                dateSent: sent_at,
                directionalDetails: Some(backup::chat_item::DirectionalDetails::Incoming(
                    backup::chat_item::IncomingMessageDetails {
                        dateReceived: sent_at,
                        read: true,
                        ..Default::default()
                    },
                )),
                item: Some(backup::chat_item::Item::StandardMessage(backup::StandardMessage {
                    text: text(body),
                    ..Default::default()
                })),
                ..Default::default()
            }))
        };
        vec![
            frame(backup::frame::Item::Recipient(backup::Recipient {
                id: 1,
                destination: Some(backup::recipient::Destination::Contact(backup::Contact {
                    aci: Some(Uuid::parse_str(ALICE).unwrap().as_bytes().to_vec()),
                    profileKey: Some(vec![3u8; 32]),
                    profileGivenName: Some("Alice".into()),
                    ..Default::default()
                })),
                ..Default::default()
            })),
            frame(backup::frame::Item::Chat(backup::Chat {
                id: 10,
                recipientId: 1,
                ..Default::default()
            })),
            item(1_000, "hello"),
            item(2_000, "again"),
        ]
    }

    async fn setup(tag: &str) -> (TempDir, ProjectionDb, SqliteStore) {
        let dir = TempDir::new(tag);
        let db = ProjectionDb::open("sqlite::memory:", None).await.unwrap();
        let store = SqliteStore::open_with_options(
            crate::actor::sqlite_options(&dir.0.join("presage.db"), &"00".repeat(32)).unwrap(),
            OnNewIdentity::Trust,
        )
        .await
        .unwrap();
        (dir, db, store)
    }

    fn waiting_state() -> HistoryImportState {
        HistoryImportState {
            stage: HistoryDbStage::Waiting,
            key: KEY.to_vec(),
            aci: ME.into(),
            cdn: None,
            cdn_key: None,
            bytes_done: 0,
            total_bytes: None,
        }
    }

    fn no_cancel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_archive_imports_end_to_end() {
        let (dir, db, mut store) = setup("e2e").await;
        let archive = dir.0.join("archive.bin");
        test_archive::write(&archive, &KEY, Uuid::parse_str(ME).unwrap(), &frames());
        db.set_history_state(&waiting_state()).await.unwrap();
        let mut fake = Fake::new(
            vec![Ok(Polled::Ready {
                cdn: 3,
                key: "k".into(),
            })],
            std::fs::read(&archive).unwrap(),
        );
        let sink = Sink(Default::default());
        let tmp = dir.0.join("history.tmp");
        let (_keep, mut cancel) = no_cancel();
        let out = run(&db, &mut store, &mut fake, &tmp, &sink, &mut cancel).await.unwrap();
        assert!(matches!(out, Outcome::Done));
        assert_eq!(db.history_state().await.unwrap(), None, "state row gone");
        assert!(!tmp.exists(), "temp file gone");
        let conversations = db.conversations().await.unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].title, "Alice");
        let contacts: Vec<_> = store.contacts().await.unwrap().collect();
        assert_eq!(contacts.len(), 1, "presage seeded");
        assert_eq!(
            sink.stages(),
            vec![
                HistoryStage::Waiting,
                HistoryStage::Downloading,
                HistoryStage::Importing,
                HistoryStage::Done
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_download_resets_for_retry() {
        let (dir, db, mut store) = setup("corrupt").await;
        let tmp = dir.0.join("history.tmp");
        std::fs::write(&tmp, vec![0u8; 4_096]).unwrap();
        let mut state = waiting_state();
        state.stage = HistoryDbStage::Downloading;
        state.cdn = Some(3);
        state.cdn_key = Some("k".into());
        state.bytes_done = 4_096;
        state.total_bytes = Some(4_096);
        db.set_history_state(&state).await.unwrap();
        let mut fake = Fake::new(vec![], vec![]);
        let (_keep, mut cancel) = no_cancel();
        let out = run(&db, &mut store, &mut fake, &tmp, &Quiet, &mut cancel).await;
        assert!(matches!(out, Err(HistoryError::Archive(_))), "got {out:?}");
        assert!(!tmp.exists(), "the bad file is gone");
        let s = db.history_state().await.unwrap().expect("state stays for Retry");
        assert_eq!(
            (s.stage, s.bytes_done, s.total_bytes, s.cdn),
            (HistoryDbStage::Downloading, 0, None, Some(3)),
            "Retry re-downloads from the start"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_upload_cleans_up_and_reports_no_history() {
        let (dir, db, mut store) = setup("noupload").await;
        db.set_history_state(&waiting_state()).await.unwrap();
        let mut fake = Fake::new(vec![Ok(Polled::NoUpload)], vec![]);
        let sink = Sink(Default::default());
        let tmp = dir.0.join("history.tmp");
        let (_keep, mut cancel) = no_cancel();
        let out = run(&db, &mut store, &mut fake, &tmp, &sink, &mut cancel).await.unwrap();
        assert!(matches!(out, Outcome::NoHistory));
        assert_eq!(db.history_state().await.unwrap(), None);
        assert_eq!(sink.stages().last(), Some(&HistoryStage::NoHistory));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_cleans_up() {
        let (dir, db, mut store) = setup("cancel").await;
        db.set_history_state(&waiting_state()).await.unwrap();
        let mut fake = Fake::new(vec![], vec![]);
        let sink = Sink(Default::default());
        let tmp = dir.0.join("history.tmp");
        std::fs::write(&tmp, b"partial").unwrap();
        let (keep, mut cancel) = no_cancel();
        keep.send(true).unwrap();
        let out = run(&db, &mut store, &mut fake, &tmp, &sink, &mut cancel).await.unwrap();
        assert!(matches!(out, Outcome::Cancelled));
        assert_eq!(db.history_state().await.unwrap(), None);
        assert!(!tmp.exists());
        assert_eq!(sink.stages().last(), Some(&HistoryStage::NoHistory));
    }
}
