//! Writes archives in the real link-and-sync format (with libsignal's own export helpers), so
//! tests prove the format, not a mock.

use libsignal_message_backup::export::{aes_cbc_encrypt, gzip_compress, hmac_checksum, pad_gzipped_bucketed};
use libsignal_message_backup::proto::backup::{BackupInfo, Frame};
use presage::libsignal_service::prelude::Uuid;
use protobuf::Message as _;

/// IV ‖ AES-256-CBC(gzip(varint-delimited BackupInfo + frames)) ‖ HMAC-SHA256, like the phone.
pub fn write(path: &std::path::Path, key: &[u8; 32], aci: Uuid, frames: &[Frame]) {
    let mbk = crate::history::read::message_backup_key(key, aci);
    let info = BackupInfo {
        version: 1,
        backupTimeMs: 1_700_000_000_000,
        ..Default::default()
    };
    let mut plain = Vec::new();
    info.write_length_delimited_to_vec(&mut plain).unwrap();
    for f in frames {
        f.write_length_delimited_to_vec(&mut plain).unwrap();
    }
    let mut body = gzip_compress(futures::io::Cursor::new(plain));
    pad_gzipped_bucketed(&mut body);
    let iv = [0x24u8; 16];
    aes_cbc_encrypt(&mbk.aes_key, &iv, &mut body);
    let hmac = hmac_checksum(&mbk.hmac_key, &iv, &body);
    let mut file = Vec::with_capacity(16 + body.len() + 32);
    file.extend(iv);
    file.extend(body);
    file.extend(hmac);
    std::fs::write(path, file).unwrap();
}
