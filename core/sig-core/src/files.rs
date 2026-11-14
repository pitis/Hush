//! The files Hush keeps from chats (attachments, chat pictures, sticker images, a draft's preview
//! image) are encrypted on disk, each under its own key in `projection.db`
//! (`ProjectionDb::kept`). A file is `MAGIC | salt (16)`, then a backup's chunks (`backup`:
//! AES-256-GCM, a MiB each, so a large video is never read whole), sealed under
//! HKDF-SHA256(salt, the file's key): each write's key is new, even for a file written again
//! under the same key. Files from before are plain until encrypted at launch
//! (`ProjectionDb::encrypt_files`).

use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::backup::{Plain, Sealer};

pub(crate) type FileKey = [u8; 32];

/// Folders beside the attachments' whose files are sealed under derived keys
/// (`ProjectionDb::derived_key`): sticker images, copies of presage's (its old folder,
/// `stickers`, held them plain), and a draft's preview image until it is sent.
pub(crate) const STICKERS: &str = "sticker-images";
pub(crate) const PREVIEWS: &str = "previews";
/// What a file from before encryption is renamed to once encrypted.
pub(crate) const ENCRYPTED: &str = ".enc";

const MAGIC: &[u8; 8] = b"HUSHFIL1";
const HEADER_LEN: usize = 8 + 16;

fn cipher_key(key: &FileKey, salt: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), key)
        .expand(b"hush file", &mut out)
        .expect("32 bytes is a valid length");
    out
}

/// A new key for a file.
pub(crate) fn new_key() -> FileKey {
    rand::random()
}

/// Writes `input` sealed under `key` to a new file at `to` (replacing one there). Returns the
/// plaintext's length.
pub(crate) fn seal(mut input: impl Read, to: &Path, key: &FileKey) -> std::io::Result<u64> {
    let salt: [u8; 16] = rand::random();
    let mut header = MAGIC.to_vec();
    header.extend_from_slice(&salt);
    let out = std::io::BufWriter::new(std::fs::File::create(to)?);
    let mut sealer = Sealer::new(out, &cipher_key(key, &salt), header)?;
    let n = std::io::copy(&mut input, &mut sealer)?;
    sealer.finish()?.into_inner().map_err(|e| e.into_error())?;
    Ok(n)
}

/// Writes the file at `path` out to the disk; with `drive`, out of the drive's own cache too, as
/// that is slow on Apple platforms (F_FULLFSYNC, milliseconds a file): a batch ends with one.
pub(crate) fn sync(path: &Path, drive: bool) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    #[cfg(target_vendor = "apple")]
    if !drive {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` is open for as long as the call
        return match unsafe { libc::fsync(file.as_raw_fd()) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        };
    }
    let _ = drive;
    file.sync_all()
}

/// The plaintext of the file at `path`, sealed under `key`, decrypted as it is read. A wrong key,
/// a changed byte or a cut-off file is an error (`InvalidData`), never other bytes.
pub(crate) fn open(path: &Path, key: &FileKey) -> std::io::Result<impl Read + Send> {
    let invalid = || std::io::Error::new(ErrorKind::InvalidData, "not an encrypted file");
    let mut input = BufReader::new(std::fs::File::open(path)?);
    let mut header = vec![0; HEADER_LEN];
    input.read_exact(&mut header).map_err(|e| match e.kind() {
        ErrorKind::UnexpectedEof => invalid(),
        _ => e,
    })?;
    if &header[..8] != MAGIC {
        return Err(invalid());
    }
    let key = cipher_key(key, &header[8..]);
    Plain::new(input, &key, header).map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))
}

/// Whether two streams hold the same bytes.
pub(crate) fn same(mut a: impl Read, mut b: impl Read) -> std::io::Result<bool> {
    let (mut x, mut y) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
    loop {
        let n = read_full(&mut a, &mut x)?;
        if n != read_full(&mut b, &mut y)? || x[..n] != y[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` as far as the stream goes.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(got) => n += got,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A file and the key it is sealed with: none for one of Hush's written before files were
/// encrypted (plain until then), or a file of the user's (one to send). No `Debug`: the key.
#[derive(Clone)]
pub(crate) struct Kept {
    pub path: PathBuf,
    pub key: Option<FileKey>,
}

impl Kept {
    /// Its plaintext, as it is read. Blocking.
    pub(crate) fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
        Ok(match &self.key {
            Some(key) => Box::new(open(&self.path, key)?),
            None => Box::new(BufReader::new(std::fs::File::open(&self.path)?)),
        })
    }

    /// Its plaintext, in memory. Blocking.
    pub(crate) fn read(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = vec![];
        self.reader()?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Its plaintext into a new file at `to`; a failed copy leaves none. Blocking.
    pub(crate) fn copy_to(&self, to: &Path) -> std::io::Result<u64> {
        let copied = self
            .reader()
            .and_then(|mut r| std::io::copy(&mut r, &mut std::fs::File::create_new(to)?));
        if copied.is_err() && to.is_file() {
            let _ = std::fs::remove_file(to);
        }
        copied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{CHUNK, TAG};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hush-sealed-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn read(path: &Path, key: &FileKey) -> std::io::Result<Vec<u8>> {
        let mut out = vec![];
        open(path, key)?.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn every_size_around_a_chunk_reads_back_and_nothing_is_in_clear() {
        let dir = temp_dir("sizes");
        let key = new_key();
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 17] {
            let path = dir.join(len.to_string());
            let plain = bytes(len);
            assert_eq!(seal(plain.as_slice(), &path, &key).unwrap(), len as u64);
            assert_eq!(read(&path, &key).unwrap(), plain, "{len}");
            let stored = std::fs::read(&path).unwrap();
            let chunks = len.div_ceil(CHUNK).max(1);
            assert_eq!(stored.len(), HEADER_LEN + len + chunks * (5 + TAG), "{len}");
            if len >= 64 {
                assert!(!stored.windows(64).any(|w| w == &plain[..64]), "{len}");
            }
        }
    }

    #[test]
    fn a_wrong_key_fails() {
        let dir = temp_dir("wrong");
        let path = dir.join("f");
        seal(&b"a photo"[..], &path, &new_key()).unwrap();
        let e = read(&path, &new_key()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn a_cut_off_or_changed_file_fails() {
        let dir = temp_dir("damage");
        let key = new_key();
        let path = dir.join("f");
        seal(bytes(2 * CHUNK + 17).as_slice(), &path, &key).unwrap();
        let stored = std::fs::read(&path).unwrap();
        // at a chunk boundary (only whole chunks left), inside one, in the header
        for cut in [HEADER_LEN + 2 * (5 + CHUNK + TAG), stored.len() - 1, 10] {
            std::fs::write(&path, &stored[..cut]).unwrap();
            assert_eq!(read(&path, &key).unwrap_err().kind(), ErrorKind::InvalidData, "{cut}");
        }
        for at in [3, 12, HEADER_LEN + 40, stored.len() - 5] {
            let mut changed = stored.clone();
            changed[at] ^= 1;
            std::fs::write(&path, &changed).unwrap();
            assert_eq!(read(&path, &key).unwrap_err().kind(), ErrorKind::InvalidData, "{at}");
        }
    }

    #[test]
    fn a_plain_file_isnt_taken_for_an_encrypted_one() {
        let dir = temp_dir("plain");
        let path = dir.join("f");
        std::fs::write(&path, bytes(100)).unwrap();
        assert_eq!(read(&path, &new_key()).unwrap_err().kind(), ErrorKind::InvalidData);
        let kept = Kept { path, key: None };
        assert_eq!(kept.read().unwrap(), bytes(100), "a file from before reads as it is");
    }

    #[test]
    fn writing_again_under_the_same_key_writes_other_bytes() {
        let dir = temp_dir("again");
        let key = new_key();
        seal(&b"same"[..], &dir.join("a"), &key).unwrap();
        seal(&b"same"[..], &dir.join("b"), &key).unwrap();
        let (a, b) = (
            std::fs::read(dir.join("a")).unwrap(),
            std::fs::read(dir.join("b")).unwrap(),
        );
        assert_ne!(a[HEADER_LEN..], b[HEADER_LEN..]);
        assert_eq!(read(&dir.join("b"), &key).unwrap(), b"same");
    }

    #[test]
    fn a_kept_file_copies_out_decrypted() {
        let dir = temp_dir("copy");
        let kept = Kept {
            path: dir.join("12"),
            key: Some(new_key()),
        };
        seal(bytes(CHUNK + 3).as_slice(), &kept.path, kept.key.as_ref().unwrap()).unwrap();
        assert_eq!(kept.copy_to(&dir.join("out")).unwrap(), CHUNK as u64 + 3);
        assert_eq!(std::fs::read(dir.join("out")).unwrap(), bytes(CHUNK + 3));
        // one that fails leaves nothing half-written
        let wrong = Kept {
            key: Some(new_key()),
            ..kept.clone()
        };
        assert!(wrong.copy_to(&dir.join("bad")).is_err());
        assert!(!dir.join("bad").exists());
    }

    #[test]
    fn same_compares_whole_streams() {
        let a = bytes(200_000);
        assert!(same(a.as_slice(), a.as_slice()).unwrap());
        assert!(!same(a.as_slice(), &a[..199_999]).unwrap());
        let mut b = a.clone();
        b[150_000] ^= 1;
        assert!(!same(a.as_slice(), b.as_slice()).unwrap());
        assert!(same(&b""[..], &b""[..]).unwrap());
    }
}
