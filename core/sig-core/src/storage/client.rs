//! The Storage Service over presage's `PushService`: the short-lived credentials, the manifest,
//! items, and the one write call libsignal-service doesn't have. Behind `StorageApi` so the
//! sync logic runs against a fake.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::Engine as _;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::master_key::StorageServiceKey;
use presage::libsignal_service::prelude::{ProtobufMessage as _, PushService};
use presage::libsignal_service::proto::{
    manifest_record, ManifestRecord, ReadOperation, StorageItem, StorageItems, StorageManifest, WriteOperation,
};
use presage::libsignal_service::push_service::{HttpAuth, HttpAuthOverride};
use presage::libsignal_service::StorageService;
use sha2::Sha256;

const IV_LEN: usize = 12;
const ITEM_KEY_INFO_PREFIX: &[u8] = b"20240801_SIGNAL_STORAGE_SERVICE_ITEM_";
/// The server reads at most 5120 keys per call; Desktop asks for 2500 at a time.
const READ_CHUNK: usize = 2500;

#[derive(Debug)]
pub(crate) enum StorageError {
    /// Another device wrote first: the manifest version moved on.
    Conflict,
    /// The server refused the credentials: nothing to do until the next connect.
    Unauthorized,
    /// A blob didn't decrypt or decode: the wrong key, or not what we expected.
    Invalid(String),
    /// The server couldn't be reached, or answered with something unexpected.
    Network(String),
    /// Our own projection failed.
    Local(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => write!(f, "another device wrote first"),
            Self::Unauthorized => write!(f, "the server refused the storage credentials"),
            Self::Invalid(what) => write!(f, "couldn't read {what}"),
            Self::Network(what) => write!(f, "{what}"),
            Self::Local(what) => write!(f, "storage: {what}"),
        }
    }
}

impl From<sqlx::Error> for StorageError {
    fn from(e: sqlx::Error) -> Self {
        Self::Local(e.to_string())
    }
}

/// A decrypted manifest: the version, the per-item key material, and every record's key with
/// its `ManifestRecord.Identifier.Type`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Manifest {
    pub version: u64,
    pub record_ikm: Vec<u8>,
    pub identifiers: Vec<(Vec<u8>, i32)>,
}

/// One write: the manifest to put at `manifest.version`, the records to insert (key, plaintext
/// `StorageRecord`), and the keys to delete.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Write {
    pub manifest: Manifest,
    pub source_device: u32,
    pub inserts: Vec<(Vec<u8>, Vec<u8>)>,
    pub deletes: Vec<Vec<u8>>,
}

/// The Storage Service calls the sync needs.
pub(crate) trait StorageApi {
    /// The manifest if the server's version differs from `version`; `None` when it is the same
    /// (or the account has none yet).
    async fn manifest_if_changed(&mut self, version: u64) -> Result<Option<Manifest>, StorageError>;
    /// The records with these keys as (key, plaintext); keys the server doesn't have are absent.
    async fn read_items(
        &mut self,
        keys: &[Vec<u8>],
        record_ikm: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StorageError>;
    async fn write(&mut self, w: &Write) -> Result<(), StorageError>;
}

fn manifest_path(version: u64) -> String {
    if version == 0 {
        "/v1/storage/manifest".into()
    } else {
        format!("/v1/storage/manifest/version/{version}")
    }
}

fn chunks(keys: &[Vec<u8>]) -> impl Iterator<Item = &[Vec<u8>]> {
    keys.chunks(READ_CHUNK)
}

fn status_error(status: u16) -> StorageError {
    match status {
        401 | 403 => StorageError::Unauthorized,
        409 => StorageError::Conflict,
        s => StorageError::Network(format!("storage: status {s}")),
    }
}

/// The per-item key, as libsignal-service derives it: HKDF off the manifest's `recordIkm` on
/// current accounts, HMAC off the storage key on old ones.
fn item_key(key: &StorageServiceKey, raw_id: &[u8], ikm: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if ikm.is_empty() {
        let mut mac = Hmac::<Sha256>::new_from_slice(&key.inner).expect("HMAC takes any key length");
        mac.update(b"Item_");
        mac.update(base64::engine::general_purpose::STANDARD.encode(raw_id).as_bytes());
        out.copy_from_slice(&mac.finalize().into_bytes());
    } else {
        Hkdf::<Sha256>::new(None, ikm)
            .expand_multi_info(&[ITEM_KEY_INFO_PREFIX, raw_id], &mut out)
            .expect("32 bytes is a valid HKDF length");
    }
    out
}

/// An item's plaintext `StorageRecord` bytes, with every field it carries (libsignal-service's
/// `decrypt_item` decodes with prost, which drops the fields our proto doesn't know).
pub(crate) fn decrypt_raw(key: &StorageServiceKey, item: &StorageItem, ikm: &[u8]) -> Result<Vec<u8>, StorageError> {
    let invalid = || StorageError::Invalid("a storage item".into());
    let (iv, ct) = item.value.split_first_chunk::<IV_LEN>().ok_or_else(invalid)?;
    Aes256Gcm::new((&item_key(key, &item.key, ikm)).into())
        .decrypt(iv.into(), ct)
        .map_err(|_| invalid())
}

/// `iv(12) || ciphertext+tag` under a fresh random IV, as every client writes items.
pub(crate) fn encrypt_raw(key: &StorageServiceKey, raw_id: &[u8], plaintext: &[u8], ikm: &[u8]) -> StorageItem {
    let iv: [u8; IV_LEN] = rand::random();
    let mut value = iv.to_vec();
    value.extend(
        Aes256Gcm::new((&item_key(key, raw_id, ikm)).into())
            .encrypt((&iv).into(), plaintext)
            .expect("AES-GCM encryption doesn't fail"),
    );
    StorageItem {
        key: raw_id.to_vec(),
        value,
    }
}

/// The `WriteOperation` for `w`: manifest and items encrypted, nothing cleared.
pub(crate) fn encode_write(key: &StorageServiceKey, w: &Write) -> WriteOperation {
    let record = ManifestRecord {
        version: w.manifest.version,
        source_device: w.source_device,
        identifiers: w
            .manifest
            .identifiers
            .iter()
            .map(|(raw, kind)| manifest_record::Identifier {
                raw: raw.clone(),
                r#type: *kind,
            })
            .collect(),
        record_ikm: w.manifest.record_ikm.clone(),
    };
    WriteOperation {
        manifest: Some(StorageService::encrypt_manifest(key, &record)),
        insert_item: w
            .inserts
            .iter()
            .map(|(id, plain)| encrypt_raw(key, id, plain, &w.manifest.record_ikm))
            .collect(),
        delete_key: w.deletes.clone(),
        clear_all: false,
    }
}

/// The real service, with this device's credentials.
pub(crate) struct StorageHttp {
    push: PushService,
    key: StorageServiceKey,
    /// `GET /v1/storage/auth`'s short-lived basic auth, fetched once per instance (one per run).
    auth: Option<HttpAuth>,
}

fn net(e: reqwest::Error) -> StorageError {
    // reqwest's error text carries the URL; keep it out of the logs
    StorageError::Network(e.without_url().to_string())
}

impl StorageHttp {
    pub(crate) fn new(data: &presage::manager::RegistrationData, key: StorageServiceKey) -> Result<Self, String> {
        Ok(Self {
            push: crate::net::push_service(data)?,
            key,
            auth: None,
        })
    }

    async fn auth(&mut self) -> Result<HttpAuth, StorageError> {
        if let Some(auth) = &self.auth {
            return Ok(auth.clone());
        }
        let resp = self
            .push
            .request(
                reqwest::Method::GET,
                Endpoint::service("/v1/storage/auth"),
                HttpAuthOverride::NoOverride,
            )
            .map_err(|e| StorageError::Network(e.to_string()))?
            .send()
            .await
            .map_err(net)?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(status_error(status));
        }
        let v: serde_json::Value = resp.json().await.map_err(net)?;
        let (Some(username), Some(password)) = (v["username"].as_str(), v["password"].as_str()) else {
            return Err(StorageError::Invalid("the storage credentials".into()));
        };
        let auth = HttpAuth {
            username: username.into(),
            password: password.into(),
        };
        self.auth = Some(auth.clone());
        Ok(auth)
    }

    /// One call on the storage host with the credentials: the status and the body.
    async fn call(
        &mut self,
        method: reqwest::Method,
        path: String,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), StorageError> {
        let auth = self.auth().await?;
        let mut req = self
            .push
            .request(method, Endpoint::storage(path), HttpAuthOverride::Identified(auth))
            .map_err(|e| StorageError::Network(e.to_string()))?;
        if let Some(body) = body {
            req = req.header("Content-Type", "application/x-protobuf").body(body);
        }
        let resp = req.send().await.map_err(net)?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(net)?.to_vec();
        Ok((status, bytes))
    }
}

impl StorageApi for StorageHttp {
    async fn manifest_if_changed(&mut self, version: u64) -> Result<Option<Manifest>, StorageError> {
        let (status, body) = self.call(reqwest::Method::GET, manifest_path(version), None).await?;
        match status {
            204 => Ok(None),
            404 => {
                tracing::info!("the account has no storage manifest");
                Ok(None)
            }
            200 => {
                let m =
                    StorageManifest::decode(&*body).map_err(|e| StorageError::Invalid(format!("the manifest: {e}")))?;
                let rec = StorageService::decrypt_manifest(&self.key, &m)
                    .map_err(|e| StorageError::Invalid(format!("the manifest: {e}")))?;
                Ok(Some(Manifest {
                    version: rec.version,
                    record_ikm: rec.record_ikm,
                    identifiers: rec.identifiers.into_iter().map(|i| (i.raw, i.r#type)).collect(),
                }))
            }
            s => Err(status_error(s)),
        }
    }

    async fn read_items(
        &mut self,
        keys: &[Vec<u8>],
        record_ikm: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StorageError> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in chunks(keys) {
            let body = ReadOperation {
                read_key: chunk.to_vec(),
            }
            .encode_to_vec();
            let (status, bytes) = self
                .call(reqwest::Method::PUT, "/v1/storage/read".into(), Some(body))
                .await?;
            if status != 200 {
                return Err(status_error(status));
            }
            let items = StorageItems::decode(&*bytes).map_err(|e| StorageError::Invalid(format!("the items: {e}")))?;
            for item in items.items {
                let plain = decrypt_raw(&self.key, &item, record_ikm)?;
                out.push((item.key, plain));
            }
        }
        Ok(out)
    }

    async fn write(&mut self, w: &Write) -> Result<(), StorageError> {
        let body = encode_write(&self.key, w).encode_to_vec();
        let (status, _) = self
            .call(reqwest::Method::PUT, "/v1/storage".into(), Some(body))
            .await?;
        match status {
            200 | 204 => Ok(()),
            s => Err(status_error(s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{storage_record::Record, ContactRecord, StorageRecord};
    use presage::libsignal_service::StorageService;

    fn key(b: u8) -> StorageServiceKey {
        StorageServiceKey::from_slice(&[b; 32]).unwrap()
    }

    fn record() -> StorageRecord {
        StorageRecord {
            record: Some(Record::Contact(ContactRecord {
                aci: "22222222-2222-2222-2222-222222222222".into(),
                archived: true,
                ..Default::default()
            })),
        }
    }

    #[test]
    fn raw_item_crypto_matches_the_library() {
        let raw_id = vec![5u8; 16];
        for ikm in [&[][..], &[3u8; 32][..]] {
            // ours → theirs
            let item = encrypt_raw(&key(7), &raw_id, &record().encode_to_vec(), ikm);
            assert_eq!(item.key, raw_id);
            let ikm_opt = (!ikm.is_empty()).then_some(ikm);
            assert_eq!(StorageService::decrypt_item(&key(7), &item, ikm_opt).unwrap(), record());
            // theirs → ours
            let theirs = StorageService::encrypt_item(&key(7), raw_id.clone(), &record(), ikm_opt);
            assert_eq!(decrypt_raw(&key(7), &theirs, ikm).unwrap(), record().encode_to_vec());
            // with a record IKM the item key comes from the IKM alone (Signal's design); on an
            // old account it comes from the storage key
            if ikm.is_empty() {
                assert!(matches!(
                    decrypt_raw(&key(8), &theirs, ikm),
                    Err(StorageError::Invalid(_))
                ));
            } else {
                assert!(matches!(
                    decrypt_raw(&key(7), &theirs, &[4u8; 32]),
                    Err(StorageError::Invalid(_))
                ));
            }
        }
    }

    #[test]
    fn manifest_paths_follow_the_version() {
        assert_eq!(manifest_path(0), "/v1/storage/manifest");
        assert_eq!(manifest_path(7), "/v1/storage/manifest/version/7");
    }

    #[test]
    fn read_keys_are_chunked_under_the_server_limit() {
        let keys: Vec<Vec<u8>> = (0..5001u32).map(|i| i.to_le_bytes().to_vec()).collect();
        let sizes: Vec<usize> = chunks(&keys).map(<[Vec<u8>]>::len).collect();
        assert_eq!(sizes, vec![2500, 2500, 1]);
        assert_eq!(chunks(&[]).count(), 0);
    }

    #[test]
    fn a_write_operation_is_encoded_as_signal_expects() {
        let ikm = vec![3u8; 32];
        let w = Write {
            manifest: Manifest {
                version: 9,
                record_ikm: ikm.clone(),
                identifiers: vec![(vec![1; 16], 1), (vec![2; 16], 4), (vec![3; 16], 8)],
            },
            source_device: 3,
            inserts: vec![(vec![1; 16], record().encode_to_vec())],
            deletes: vec![vec![0; 16]],
        };
        let op = encode_write(&key(7), &w);
        assert!(!op.clear_all);
        assert_eq!(op.delete_key, vec![vec![0; 16]]);
        let manifest = StorageService::decrypt_manifest(&key(7), op.manifest.as_ref().unwrap()).unwrap();
        assert_eq!(
            (manifest.version, manifest.source_device, manifest.record_ikm.as_slice()),
            (9, 3, &ikm[..])
        );
        let ids: Vec<(Vec<u8>, i32)> = manifest.identifiers.iter().map(|i| (i.raw.clone(), i.r#type)).collect();
        assert_eq!(ids, w.manifest.identifiers);
        assert_eq!(op.insert_item.len(), 1);
        assert_eq!(op.insert_item[0].key, vec![1; 16]);
        assert_eq!(
            StorageService::decrypt_item(&key(7), &op.insert_item[0], Some(&ikm)).unwrap(),
            record()
        );
    }

    #[test]
    fn statuses_map_to_errors() {
        assert!(matches!(status_error(401), StorageError::Unauthorized));
        assert!(matches!(status_error(403), StorageError::Unauthorized));
        assert!(matches!(status_error(409), StorageError::Conflict));
        assert!(matches!(status_error(500), StorageError::Network(m) if m.contains("500")));
    }
}
