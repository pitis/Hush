//! Waiting for the phone's history archive and downloading it, with resume.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use futures::io::AsyncRead;
use futures::AsyncReadExt as _;
use tokio::sync::watch;

use crate::events::{CoreEvent, EventListener};
use crate::history::{HistoryError, HistoryStage};
use crate::projection::{HistoryDbStage, HistoryImportState, ProjectionDb};

/// One long poll's server-side wait (the HTTP client's reads time out at 65 s).
const POLL_SECS: u32 = 60;
/// After a failed poll or a cut download, wait this long before trying again.
const RETRY: Duration = Duration::from_secs(10);
/// How often download progress is persisted and reported (bytes).
const PROGRESS_EVERY: u64 = 4 * 1024 * 1024;

/// One answer of `GET /v1/devices/transfer_archive`.
pub(crate) enum Polled {
    NotYet,
    Ready {
        cdn: u32,
        key: String,
    },
    /// The phone gave up (CONTINUE_WITHOUT_UPLOAD) or wants a fresh relink (RELINK_REQUESTED).
    NoUpload,
}

pub(crate) struct DownloadStart {
    /// Where the returned bytes start; 0 if the server ignored the requested offset.
    pub offset: u64,
    pub total: Option<u64>,
    pub body: Pin<Box<dyn AsyncRead>>,
}

/// The server side of the transfer, so tests can script its answers.
pub(crate) trait ArchiveServer {
    /// One long poll, waiting up to `timeout_secs` on the server.
    async fn wait(&mut self, timeout_secs: u32) -> Result<Polled, HistoryError>;
    /// Starts (or resumes, from `offset`) the archive download. `None`: the archive is gone
    /// from the CDN (it expired) — not retryable.
    async fn download(&mut self, cdn: u32, key: &str, offset: u64) -> Result<Option<DownloadStart>, HistoryError>;
}

pub(crate) enum Fetched {
    File,
    NoUpload,
    Cancelled,
}

enum Once {
    Progressed,
    Cancelled,
    Gone,
}

/// Drives wait + download, keeping `history_import` current and emitting `HistoryProgress`,
/// until the archive is fully on disk at `tmp` (or there is none, or the user cancelled).
pub(crate) async fn fetch_archive(
    server: &mut impl ArchiveServer,
    db: &ProjectionDb,
    tmp: &Path,
    l: &dyn EventListener,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Fetched, HistoryError> {
    let mut state = db
        .history_state()
        .await?
        .ok_or_else(|| HistoryError::Server("no import state".into()))?;
    // Wait for the phone's upload, unless a previous run already learned where it is.
    while state.cdn.is_none() {
        l.on_event(CoreEvent::HistoryProgress {
            stage: HistoryStage::Waiting,
            done: 0,
            total: 0,
        });
        if *cancel.borrow() {
            return Ok(Fetched::Cancelled);
        }
        let polled = tokio::select! {
            p = server.wait(POLL_SECS) => p,
            _ = cancelled(cancel) => return Ok(Fetched::Cancelled),
        };
        match polled {
            Ok(Polled::Ready { cdn, key }) => {
                state.stage = HistoryDbStage::Downloading;
                state.cdn = Some(cdn);
                state.cdn_key = Some(key);
                db.set_history_state(&state).await?;
            }
            Ok(Polled::NoUpload) => return Ok(Fetched::NoUpload),
            Ok(Polled::NotYet) => {}
            Err(e) => {
                tracing::warn!("waiting for the archive failed: {e}; retrying");
                if pause(RETRY, cancel).await {
                    return Ok(Fetched::Cancelled);
                }
            }
        }
    }
    let (cdn, key) = (state.cdn.unwrap(), state.cdn_key.clone().unwrap_or_default());
    // Download, resuming, until the file is complete.
    loop {
        if state.total_bytes.is_some_and(|t| t > 0 && state.bytes_done >= t) {
            state.stage = HistoryDbStage::Importing;
            db.set_history_state(&state).await?;
            return Ok(Fetched::File);
        }
        if *cancel.borrow() {
            return Ok(Fetched::Cancelled);
        }
        match download_once(server, db, tmp, l, cancel, &mut state, cdn, &key).await {
            Ok(Once::Progressed) => {}
            Ok(Once::Cancelled) => return Ok(Fetched::Cancelled),
            Ok(Once::Gone) => return Ok(Fetched::NoUpload),
            Err(e) => {
                tracing::warn!("downloading the archive failed: {e}; retrying");
                if pause(RETRY, cancel).await {
                    return Ok(Fetched::Cancelled);
                }
            }
        }
    }
}

/// One download attempt: writes from the server's offset, persisting progress as it goes.
#[allow(clippy::too_many_arguments)]
async fn download_once(
    server: &mut impl ArchiveServer,
    db: &ProjectionDb,
    tmp: &Path,
    l: &dyn EventListener,
    cancel: &mut watch::Receiver<bool>,
    state: &mut HistoryImportState,
    cdn: u32,
    key: &str,
) -> Result<Once, HistoryError> {
    let io = |e: std::io::Error| HistoryError::Download(e.to_string());
    let Some(start) = server.download(cdn, key, state.bytes_done).await? else {
        return Ok(Once::Gone);
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(tmp)
        .map_err(io)?;
    if start.offset != state.bytes_done {
        // the server ignored the resume: whatever is on disk is stale
        file.set_len(start.offset).map_err(io)?;
        state.bytes_done = start.offset;
    }
    file.seek(SeekFrom::Start(start.offset)).map_err(io)?;
    if start.total.is_some() {
        state.total_bytes = start.total;
    }
    state.stage = HistoryDbStage::Downloading;
    db.set_history_state(state).await?;
    let mut body = start.body;
    let mut buf = vec![0u8; 64 * 1024];
    let mut unsaved = 0u64;
    loop {
        if *cancel.borrow() {
            db.set_history_state(state).await?;
            return Ok(Once::Cancelled);
        }
        let n = match body.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                // keep what arrived, so the next round resumes here
                db.set_history_state(state).await?;
                return Err(io(e));
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(io)?;
        state.bytes_done += n as u64;
        unsaved += n as u64;
        if unsaved >= PROGRESS_EVERY {
            unsaved = 0;
            db.set_history_state(state).await?;
            l.on_event(CoreEvent::HistoryProgress {
                stage: HistoryStage::Downloading,
                done: state.bytes_done,
                total: state.total_bytes.unwrap_or(0),
            });
        }
    }
    file.flush().map_err(io)?;
    if state.total_bytes.is_none() {
        // no Content-Length and the stream ended: take that as the whole file (the HMAC check
        // catches a lie)
        state.total_bytes = Some(state.bytes_done);
    }
    db.set_history_state(state).await?;
    l.on_event(CoreEvent::HistoryProgress {
        stage: HistoryStage::Downloading,
        done: state.bytes_done,
        total: state.total_bytes.unwrap_or(0),
    });
    Ok(Once::Progressed)
}

/// The production server, authenticated as this device.
pub(crate) struct SignalServer {
    push: presage::libsignal_service::prelude::PushService,
}

impl SignalServer {
    /// Authenticated with this device's credentials. presage keeps the account password
    /// private, but `RegistrationData` serializes it — that is how presage itself persists it.
    pub(crate) fn from_registration(data: &presage::manager::RegistrationData) -> Result<Self, HistoryError> {
        use presage::libsignal_service::configuration::{ServiceCredentials, SignalServers};
        use presage::libsignal_service::prelude::{phonenumber, PushService};
        use presage::libsignal_service::protocol::E164;
        use std::str::FromStr as _;
        let err = |m: String| HistoryError::Server(m);
        let v = serde_json::to_value(data).map_err(|e| err(e.to_string()))?;
        let password = v["password"]
            .as_str()
            .ok_or_else(|| err("no account password".into()))?
            .to_owned();
        let e164 = data.phone_number.format().mode(phonenumber::Mode::E164).to_string();
        let credentials = ServiceCredentials {
            aci: Some(data.service_ids.aci),
            pni: Some(data.service_ids.pni),
            phonenumber: E164::from_str(&e164).map_err(|e| err(format!("phone number: {e}")))?,
            password: Some(password),
            device_id: data.device_id.and_then(|d| d.try_into().ok()),
        };
        Ok(Self {
            push: PushService::new(SignalServers::Production, Some(credentials), "Semaphore"),
        })
    }
}

impl ArchiveServer for SignalServer {
    async fn wait(&mut self, timeout_secs: u32) -> Result<Polled, HistoryError> {
        use presage::libsignal_service::configuration::Endpoint;
        use presage::libsignal_service::push_service::HttpAuthOverride;
        let err = |m: String| HistoryError::Server(m);
        let resp = self
            .push
            .request(
                reqwest::Method::GET,
                Endpoint::service(format!("/v1/devices/transfer_archive?timeout={timeout_secs}")),
                HttpAuthOverride::NoOverride,
            )
            .map_err(|e| err(e.to_string()))?
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        match resp.status().as_u16() {
            204 => Ok(Polled::NotYet),
            200 => {
                let v: serde_json::Value = resp.json().await.map_err(|e| err(e.to_string()))?;
                if let Some(error) = v["error"].as_str() {
                    // RELINK_REQUESTED or CONTINUE_WITHOUT_UPLOAD: either way, no archive comes
                    tracing::warn!(error, "the phone didn't upload the archive");
                    return Ok(Polled::NoUpload);
                }
                match (v["cdn"].as_u64(), v["key"].as_str()) {
                    (Some(cdn), Some(key)) => Ok(Polled::Ready {
                        cdn: cdn as u32,
                        key: key.into(),
                    }),
                    _ => Err(err("unexpected transfer_archive answer".into())),
                }
            }
            s => Err(err(format!("transfer_archive: HTTP {s}"))),
        }
    }

    async fn download(&mut self, cdn: u32, key: &str, offset: u64) -> Result<Option<DownloadStart>, HistoryError> {
        use futures::TryStreamExt as _;
        use presage::libsignal_service::configuration::Endpoint;
        use presage::libsignal_service::push_service::HttpAuthOverride;
        let err = |m: String| HistoryError::Download(m);
        let resp = self
            .push
            .request(
                reqwest::Method::GET,
                Endpoint::cdn(cdn, format!("/attachments/{key}")),
                HttpAuthOverride::Unidentified, // CDN downloads are unauthenticated
            )
            .map_err(|e| err(e.to_string()))?
            .header(reqwest::header::RANGE, format!("bytes={offset}-"))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        let (starts_at, total) = match resp.status().as_u16() {
            // Content-Range: `bytes <from>-<to>/<total>`
            206 => {
                let total = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|t| t.parse().ok());
                (offset, total)
            }
            200 => (0, resp.content_length()),
            // we asked past the end: it is already fully here
            416 if offset > 0 => {
                return Ok(Some(DownloadStart {
                    offset,
                    total: Some(offset),
                    body: Box::pin(futures::io::empty()),
                }));
            }
            // the archive expired from the CDN: waiting longer can't help
            404 => return Ok(None),
            s => return Err(err(format!("archive download: HTTP {s}"))),
        };
        let body = resp.bytes_stream().map_err(std::io::Error::other).into_async_read();
        Ok(Some(DownloadStart {
            offset: starts_at,
            total,
            body: Box::pin(body),
        }))
    }
}

/// Waits until cancellation is asked for (a closed channel counts: the core is going away).
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Waits `retry` before trying again, or returns true if cancelled meanwhile.
async fn pause(retry: Duration, cancel: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(retry) => false,
        _ = cancelled(cancel) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::events::{CoreEvent, EventListener};

    struct Quiet;
    impl EventListener for Quiet {
        fn on_event(&self, _: CoreEvent) {}
    }

    /// Plays a script: each `wait` answer in turn (hanging when it runs out), then serves
    /// `file`, honoring (or ignoring) offsets and cutting each download after `cut_after` bytes.
    struct Fake {
        waits: Vec<Result<Polled, HistoryError>>,
        file: Vec<u8>,
        cut_after: Option<usize>,
        honors_range: bool,
        gone: bool,
        offsets_seen: Mutex<Vec<u64>>,
    }

    impl Fake {
        fn new(waits: Vec<Result<Polled, HistoryError>>, file: Vec<u8>) -> Self {
            Self {
                waits,
                file,
                cut_after: None,
                honors_range: true,
                gone: false,
                offsets_seen: Mutex::new(vec![]),
            }
        }
    }

    impl ArchiveServer for Fake {
        async fn wait(&mut self, _timeout: u32) -> Result<Polled, HistoryError> {
            if self.waits.is_empty() {
                futures::future::pending().await
            } else {
                self.waits.remove(0)
            }
        }

        async fn download(
            &mut self,
            _cdn: u32,
            _key: &str,
            offset: u64,
        ) -> Result<Option<DownloadStart>, HistoryError> {
            self.offsets_seen.lock().unwrap().push(offset);
            if self.gone {
                return Ok(None);
            }
            let start = if self.honors_range { offset as usize } else { 0 };
            let end = self
                .cut_after
                .map_or(self.file.len(), |n| (start + n).min(self.file.len()));
            Ok(Some(DownloadStart {
                offset: start as u64,
                total: Some(self.file.len() as u64),
                body: Box::pin(futures::io::Cursor::new(self.file[start..end].to_vec())),
            }))
        }
    }

    async fn db() -> ProjectionDb {
        ProjectionDb::open("sqlite::memory:", None).await.unwrap()
    }

    fn state() -> HistoryImportState {
        HistoryImportState {
            stage: HistoryDbStage::Waiting,
            key: vec![1u8; 32],
            aci: "22222222-2222-2222-2222-222222222222".into(),
            cdn: None,
            cdn_key: None,
            bytes_done: 0,
            total_bytes: None,
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sig-fetch-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.tmp");
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn waits_until_ready_then_downloads_all() {
        let db = db().await;
        db.set_history_state(&state()).await.unwrap();
        let mut fake = Fake::new(
            vec![
                Ok(Polled::NotYet),
                Ok(Polled::Ready {
                    cdn: 3,
                    key: "k".into(),
                }),
            ],
            vec![5u8; 10_000],
        );
        let path = tmp("full");
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut cancel).await.unwrap();
        assert!(matches!(got, Fetched::File));
        assert_eq!(std::fs::read(&path).unwrap(), vec![5u8; 10_000]);
        let s = db.history_state().await.unwrap().unwrap();
        assert_eq!(
            (s.stage, s.bytes_done, s.total_bytes, s.cdn, s.cdn_key.as_deref()),
            (HistoryDbStage::Importing, 10_000, Some(10_000), Some(3), Some("k"))
        );
    }

    #[tokio::test]
    async fn a_cut_download_resumes_at_the_right_byte() {
        let db = db().await;
        db.set_history_state(&state()).await.unwrap();
        let file: Vec<u8> = (0..=255u8).cycle().take(9_000).collect();
        let mut fake = Fake::new(
            vec![Ok(Polled::Ready {
                cdn: 3,
                key: "k".into(),
            })],
            file.clone(),
        );
        fake.cut_after = Some(4_000);
        let path = tmp("resume");
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut cancel).await.unwrap();
        assert!(matches!(got, Fetched::File));
        assert_eq!(std::fs::read(&path).unwrap(), file);
        assert_eq!(*fake.offsets_seen.lock().unwrap(), vec![0, 4_000, 8_000]);
    }

    #[tokio::test]
    async fn a_server_that_ignores_range_restarts_the_file() {
        let db = db().await;
        let mut s = state();
        s.stage = HistoryDbStage::Downloading;
        s.cdn = Some(3);
        s.cdn_key = Some("k".into());
        s.bytes_done = 4_000; // a previous run got this far
        s.total_bytes = Some(9_000);
        db.set_history_state(&s).await.unwrap();
        let path = tmp("norange");
        std::fs::write(&path, vec![9u8; 4_000]).unwrap(); // stale partial content
        let mut fake = Fake::new(vec![], vec![7u8; 9_000]);
        fake.honors_range = false;
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut cancel).await.unwrap();
        assert!(matches!(got, Fetched::File));
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 9_000], "no stale prefix");
    }

    #[tokio::test]
    async fn a_complete_file_skips_the_network() {
        let db = db().await;
        let mut s = state();
        s.stage = HistoryDbStage::Downloading;
        s.cdn = Some(3);
        s.cdn_key = Some("k".into());
        s.bytes_done = 5_000;
        s.total_bytes = Some(5_000);
        db.set_history_state(&s).await.unwrap();
        let path = tmp("done");
        std::fs::write(&path, vec![1u8; 5_000]).unwrap();
        let mut fake = Fake::new(vec![Err(HistoryError::Server("must not be called".into()))], vec![]);
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &path, &Quiet, &mut cancel).await.unwrap();
        assert!(matches!(got, Fetched::File));
        assert!(fake.offsets_seen.lock().unwrap().is_empty(), "no download either");
        assert_eq!(fake.waits.len(), 1, "no wait either");
    }

    #[tokio::test]
    async fn the_phone_giving_up_ends_the_wait() {
        let db = db().await;
        db.set_history_state(&state()).await.unwrap();
        let mut fake = Fake::new(vec![Ok(Polled::NoUpload)], vec![]);
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &tmp("noupload"), &Quiet, &mut cancel)
            .await
            .unwrap();
        assert!(matches!(got, Fetched::NoUpload));
    }

    #[tokio::test]
    async fn a_vanished_archive_means_no_history() {
        let db = db().await;
        db.set_history_state(&state()).await.unwrap();
        let mut fake = Fake::new(
            vec![Ok(Polled::Ready {
                cdn: 3,
                key: "k".into(),
            })],
            vec![1, 2, 3],
        );
        fake.gone = true;
        let (_keep, mut cancel) = watch::channel(false);
        let got = fetch_archive(&mut fake, &db, &tmp("gone"), &Quiet, &mut cancel)
            .await
            .unwrap();
        assert!(matches!(got, Fetched::NoUpload));
    }

    #[tokio::test]
    async fn a_wait_error_is_retried_and_cancel_ends_it() {
        let db = db().await;
        db.set_history_state(&state()).await.unwrap();
        let (cancel_tx, mut cancel) = watch::channel(false);
        let mut fake = Fake::new(
            vec![Err(HistoryError::Server("boom".into())), Ok(Polled::NotYet)],
            vec![],
        );
        // paused only now: the pool's connection is already open, so no sqlx timer races the clock
        tokio::time::pause();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            let _ = cancel_tx.send(true);
        });
        let got = fetch_archive(&mut fake, &db, &tmp("cancel"), &Quiet, &mut cancel)
            .await
            .unwrap();
        assert!(matches!(got, Fetched::Cancelled));
    }
}
