//! Downloading attachments: one at a time on the core thread, each reported as it moves from
//! Pending to Downloading to Ready (or Failed, and the next one goes on).

use std::future::Future;
use std::time::Duration;

use presage::libsignal_service::prelude::ProtobufMessage;
use presage::manager::Registered;
use presage::proto::AttachmentPointer;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::mpsc;

use crate::events::{CoreEvent, EventListener};
use crate::projection::ProjectionDb;

/// What to download.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Download {
    /// Whatever downloads by itself (small images and voice notes) and isn't there yet.
    Auto,
    /// One attachment the user asked for.
    One(i64),
}

/// A download that takes longer is given up (Failed; the user can try again).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Downloads as asked, in order, for as long as the channel is open.
pub async fn downloader(
    m: Manager<SqliteStore, Registered>,
    db: ProjectionDb,
    l: std::sync::Arc<dyn EventListener>,
    mut jobs: mpsc::UnboundedReceiver<Download>,
) {
    let fetch = |p: AttachmentPointer| {
        let m = m.clone();
        async move {
            match tokio::time::timeout(DOWNLOAD_TIMEOUT, Box::pin(m.get_attachment(&p))).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("timed out".to_string()),
            }
        }
    };
    while let Some(job) = jobs.recv().await {
        run(&db, l.as_ref(), job, &fetch).await;
    }
}

pub async fn run<F, Fut>(db: &ProjectionDb, l: &dyn EventListener, job: Download, fetch: &F)
where
    F: Fn(AttachmentPointer) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, String>>,
{
    let ids = match job {
        Download::One(id) => vec![id],
        Download::Auto => match db.auto_downloads().await {
            Ok(ids) => ids,
            Err(e) => {
                l.on_event(CoreEvent::Error {
                    message: format!("finding attachments to download failed: {e}"),
                });
                return;
            }
        },
    };
    for id in ids {
        download(db, l, id, fetch).await;
    }
}

async fn download<F, Fut>(db: &ProjectionDb, l: &dyn EventListener, id: i64, fetch: &F)
where
    F: Fn(AttachmentPointer) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, String>>,
{
    let report = |conversation_id: String, message| {
        l.on_event(CoreEvent::AttachmentChanged {
            conversation_id,
            message,
            id,
        })
    };
    let (conversation_id, message, pointer) = match db.start_download(id).await {
        Ok(Some(started)) => started,
        Ok(None) => return, // already there, downloading, or gone
        Err(e) => {
            l.on_event(CoreEvent::Error {
                message: format!("starting a download failed: {e}"),
            });
            return;
        }
    };
    report(conversation_id, message);
    let data = match AttachmentPointer::decode(pointer.as_slice()) {
        Ok(p) => fetch(p).await,
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = &data {
        tracing::warn!(id, error = %e, "attachment download failed");
    }
    match db.finish_download(id, data).await {
        Ok(Some((conversation_id, message))) => report(conversation_id, message),
        Ok(None) => {}
        Err(e) => l.on_event(CoreEvent::Error {
            message: format!("storing a download failed: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::projection::{AttachmentState, Message, MessageKey, MessageStatus, NewAttachment, Op};

    struct Sink(Mutex<Vec<CoreEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: CoreEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    struct TempFiles(std::path::PathBuf);
    impl Drop for TempFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn photo(file_name: &str) -> NewAttachment {
        NewAttachment {
            pointer: AttachmentPointer {
                file_name: Some(file_name.into()),
                ..Default::default()
            }
            .encode_to_vec(),
            content_type: "image/jpeg".into(),
            file_name: Some(file_name.into()),
            size: 3,
            width: None,
            height: None,
            is_voice_note: false,
        }
    }

    /// The pretend server: "bad" fails its digest check, the rest download.
    async fn fetch(p: AttachmentPointer) -> Result<Vec<u8>, String> {
        match p.file_name.as_deref() {
            Some("bad") => Err("unexpected attachment checksum".into()),
            name => Ok(name.unwrap_or_default().as_bytes().to_vec()),
        }
    }

    #[tokio::test]
    async fn a_corrupt_download_fails_and_the_others_still_arrive() {
        let files = TempFiles(std::env::temp_dir().join(format!("semaphore-dl-{}", std::process::id())));
        let db = ProjectionDb::open("sqlite::memory:", None)
            .await
            .unwrap()
            .with_files(files.0.clone());
        let key = MessageKey {
            author: "alice".into(),
            sent_at: 1,
        };
        db.insert_message(&Message {
            conversation_id: "c:alice".into(),
            author: "alice".into(),
            sent_at: 1,
            received_at: 1,
            body: None,
            is_outgoing: false,
            status: MessageStatus::Delivered,
            edited: false,
            deleted: false,
            quote: None,
        })
        .await
        .unwrap();
        db.apply(&Op::AddAttachments {
            message: key.clone(),
            attachments: vec![photo("one"), photo("bad"), photo("two")],
            preview: None,
        })
        .await
        .unwrap();
        let sink = Sink(Mutex::new(vec![]));
        run(&db, &sink, Download::Auto, &fetch).await;
        let got = db.attachments(&key).await.unwrap();
        assert_eq!(
            got.iter().map(|a| a.state).collect::<Vec<_>>(),
            [AttachmentState::Ready, AttachmentState::Failed, AttachmentState::Ready]
        );
        assert_eq!(std::fs::read(got[2].path.as_ref().unwrap()).unwrap(), b"two");
        // each one is reported when it starts and when it ends
        let changes = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(
                |e| matches!(e, CoreEvent::AttachmentChanged { conversation_id, .. } if conversation_id == "c:alice"),
            )
            .count();
        assert_eq!(changes, 6);
        // the user asks again for the failed one; auto never retries it by itself
        run(&db, &sink, Download::Auto, &fetch).await;
        assert_eq!(
            db.attachment(got[1].id).await.unwrap().unwrap().state,
            AttachmentState::Failed
        );
        run(&db, &sink, Download::One(got[1].id), &fetch).await;
        assert_eq!(
            db.attachment(got[1].id).await.unwrap().unwrap().state,
            AttachmentState::Failed
        );
    }
}
