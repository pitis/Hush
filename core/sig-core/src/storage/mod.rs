//! Chat settings follow the phone through Signal's Storage Service: the account's encrypted,
//! server-stored records (contacts, groups, the account) that every device reads and writes.
//! Spec: `docs/superpowers/specs/2026-10-16-m4-storage-sync-design.md`.

use presage::libsignal_service::content::{sync_message, ContentBody, SyncMessage};
use presage::libsignal_service::master_key::StorageServiceKey;
use presage::store::StateStore;
use presage_store_sqlite::SqliteStore;
use sha2::{Digest, Sha256};

use crate::projection::{ChatFlags, ProjectionDb};
use client::{StorageError, StorageHttp};
use sync::{Keys, RunOutcome};

pub(crate) mod client;
pub(crate) mod records;
pub(crate) mod sync;

/// The account record's `ManifestRecord.Identifier.Type` (the one kind we look for by type).
pub(crate) const KIND_ACCOUNT: i32 = 4;

/// What a storage record means to us.
#[derive(Debug, PartialEq)]
pub(crate) enum Remote {
    /// A contact's or group's chat and its flags.
    Chat { conversation_id: String, flags: ChatFlags },
    /// The account: the pinned list, and Note to Self's (archived, marked unread).
    Account { pins: Vec<Pin>, note_to_self: (bool, bool) },
    /// A record we don't interpret (a contact without an ACI, an unknown type…): kept as is.
    Other,
}

/// One entry of the account's pinned list.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Pin {
    /// A chat we can name (`c:<aci>` or `g:<hex master key>`).
    Chat(String),
    /// An entry we can't map (an e164-only contact…): its encoded `PinnedConversation`, carried
    /// unchanged when we write.
    Raw(Vec<u8>),
}

/// Why a sync runs: the debounce applies to local changes only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Trigger {
    /// Caught up with the server, or a new key arrived.
    Connected,
    /// The phone says its storage changed.
    Phone,
    /// A flag changed here, or writes were turned on.
    Local,
}

/// The phone's "my storage changed, fetch the manifest" sync message.
pub(crate) fn is_storage_fetch_latest(body: &ContentBody) -> bool {
    matches!(
        body,
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::FetchLatest(f)),
            ..
        }) if f.r#type == Some(sync_message::fetch_latest::Type::StorageManifest as i32)
    )
}

/// The phone's keys (presage stores the master key from it).
pub(crate) fn is_keys_sync(body: &ContentBody) -> bool {
    matches!(
        body,
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(sync_message::Content::Keys(_)),
            ..
        })
    )
}

/// One sync with the real service, under the master key presage holds. `Ok(None)`: no master
/// key yet (the phone hasn't sent it), nothing to do.
pub(crate) async fn sync_once(
    store: &SqliteStore,
    db: &ProjectionDb,
    data: &presage::manager::RegistrationData,
) -> Result<Option<RunOutcome>, StorageError> {
    let master = store
        .fetch_master_key()
        .await
        .map_err(|e| StorageError::Local(format!("reading the master key: {e}")))?;
    let Some(master) = master else {
        tracing::info!("storage sync skipped: no master key yet");
        return Ok(None);
    };
    let key = StorageServiceKey::from_master_key(&master);
    let keys = Keys {
        fingerprint: Sha256::digest(key.inner).to_vec(),
    };
    let mut http = StorageHttp::new(data, key).map_err(StorageError::Network)?;
    let me = data.service_ids.aci.to_string();
    let device_id = data.device_id.unwrap_or(1);
    sync::run(&mut http, db, &keys, &me, device_id, tokio::time::sleep)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::DataMessage;

    fn sync(content: sync_message::Content) -> ContentBody {
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(content),
            ..Default::default()
        })
    }

    #[test]
    fn only_the_storage_manifest_fetch_latest_counts() {
        let fetch = |t: sync_message::fetch_latest::Type| {
            sync(sync_message::Content::FetchLatest(sync_message::FetchLatest {
                r#type: Some(t as i32),
            }))
        };
        assert!(is_storage_fetch_latest(&fetch(
            sync_message::fetch_latest::Type::StorageManifest
        )));
        assert!(!is_storage_fetch_latest(&fetch(
            sync_message::fetch_latest::Type::LocalProfile
        )));
        assert!(!is_storage_fetch_latest(&sync(sync_message::Content::Sent(
            Default::default()
        ))));
        assert!(!is_storage_fetch_latest(&ContentBody::DataMessage(
            DataMessage::default()
        )));
    }

    #[test]
    fn a_keys_sync_is_recognised() {
        assert!(is_keys_sync(&sync(sync_message::Content::Keys(Default::default()))));
        assert!(!is_keys_sync(&sync(sync_message::Content::Sent(Default::default()))));
        assert!(!is_keys_sync(&ContentBody::DataMessage(DataMessage::default())));
    }
}
