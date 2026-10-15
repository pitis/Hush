//! Verifies, decrypts and streams a downloaded archive as decoded frames, off the core thread.

use std::path::PathBuf;

use libsignal_message_backup::frame::{FileReaderFactory, FramesReader};
use libsignal_message_backup::key::MessageBackupKey;
use libsignal_message_backup::parse::VarintDelimitedReader;
use libsignal_message_backup::proto::backup::{BackupInfo, Frame};
use presage::libsignal_service::prelude::Uuid;
use protobuf::Message as _;
use tokio::sync::mpsc;

/// How many decoded frames may wait between the blocking reader and the importer.
const CHANNEL: usize = 256;

/// Derives the archive's key from the ephemeral backup key and our ACI (link-and-sync has no
/// forward-secrecy token).
pub(crate) fn message_backup_key(key: &[u8; 32], aci: Uuid) -> MessageBackupKey {
    use presage::libsignal_service::libsignal_account_keys::BackupKey;
    use presage::libsignal_service::protocol::Aci;
    // the bare type pins the default (latest) key version
    let backup_key: BackupKey = BackupKey(*key);
    let id = backup_key.derive_backup_id(&Aci::from(aci));
    MessageBackupKey::derive(&backup_key, &id, None)
}

/// Streams the archive's frames. The file's HMAC is verified over the whole file before the
/// first frame is sent. An `Err` item reports a failure (HMAC, decryption, gzip, protobuf) and
/// ends the stream; a closed channel without one is the end of the archive.
pub(crate) fn stream(path: PathBuf, key: [u8; 32], aci: Uuid) -> mpsc::Receiver<Result<Frame, String>> {
    let (tx, rx) = mpsc::channel(CHANNEL);
    // AES + gzip are CPU work: keep them off the core thread.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = futures::executor::block_on(read_all(&path, &key, aci, &tx)) {
            let _ = tx.blocking_send(Err(e));
        }
    });
    rx
}

async fn read_all(
    path: &std::path::Path,
    key: &[u8; 32],
    aci: Uuid,
    tx: &mpsc::Sender<Result<Frame, String>>,
) -> Result<(), String> {
    let mbk = message_backup_key(key, aci);
    let reader = FramesReader::new(&mbk, FileReaderFactory { path })
        .await
        .map_err(|e| e.to_string())?;
    let mut frames = VarintDelimitedReader::new(reader);
    // The first item is the BackupInfo header.
    let info = frames
        .read_next()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("empty archive")?;
    let info = BackupInfo::parse_from_bytes(&info).map_err(|e| e.to_string())?;
    tracing::info!(version = info.version, "reading history archive");
    while let Some(bytes) = frames.read_next().await.map_err(|e| e.to_string())? {
        let frame = Frame::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
        if tx.blocking_send(Ok(frame)).is_err() {
            return Ok(()); // the importer went away (cancelled): stop reading
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::test_archive;
    use libsignal_message_backup::proto::backup::{frame, ChatItem, Frame};

    const KEY: [u8; 32] = [9; 32];

    fn aci() -> Uuid {
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
    }

    fn chat_item_frame(date_sent: u64) -> Frame {
        let item = ChatItem {
            dateSent: date_sent,
            ..Default::default()
        };
        Frame {
            item: Some(frame::Item::ChatItem(item)),
            ..Default::default()
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sig-history-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.tmp")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn frames_round_trip() {
        let path = tmp("roundtrip");
        test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1), chat_item_frame(2)]);
        let mut rx = stream(path, KEY, aci());
        let mut seen = vec![];
        while let Some(f) = rx.recv().await {
            seen.push(f.expect("valid archive"));
        }
        let sent: Vec<u64> = seen
            .iter()
            .map(|f| match &f.item {
                Some(frame::Item::ChatItem(c)) => c.dateSent,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(sent, vec![1, 2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flipped_byte_fails_before_any_frame() {
        let path = tmp("flipped");
        test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let mut rx = stream(path, KEY, aci());
        let first = rx.recv().await;
        assert!(matches!(first, Some(Err(_))), "got {first:?}");
        assert!(rx.recv().await.is_none(), "nothing after the error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_truncated_file_fails() {
        let path = tmp("truncated");
        test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let mut rx = stream(path, KEY, aci());
        assert!(matches!(rx.recv().await, Some(Err(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_key_fails() {
        let path = tmp("wrongkey");
        test_archive::write(&path, &KEY, aci(), &[chat_item_frame(1)]);
        let mut rx = stream(path, [1; 32], aci());
        assert!(matches!(rx.recv().await, Some(Err(_))));
    }
}
