//! Importing the phone's message history at link time (link-and-sync).

// The pieces land one by one and are wired together by the driver; until then they are unused.
// TODO(remove when the driver lands): drop this allow.
#![allow(dead_code)]

pub(crate) mod fetch;
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
