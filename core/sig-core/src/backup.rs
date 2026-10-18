//! Encrypted backups of what this Mac shows: the projection and its files, under a passphrase,
//! so history moves to a new Mac (restored before linking there).
//!
//! The file is a header, `MAGIC | salt (16) | Argon2id memory KiB, passes, lanes (u32 LE each)`,
//! then chunks of at most `CHUNK` plaintext bytes: `last (u8) | length (u32 LE) | AES-256-GCM`,
//! the nonce being the chunk's number (u64 BE) and `last` (u32 BE), the header the associated
//! data. A missing last chunk (a cut-off file) or one out of order fails to decrypt.
//!
//! Inside, entries `name length (u16 LE) | name | size (u64 LE) | bytes`, ended by a zero length:
//! `MANIFEST` first, then `DATABASE` and the files as `attachments/<name>`. The manifest carries
//! the database copy's key, so it is never written out on restore.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use std::io::{Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"HUSHBAK1";
const HEADER_LEN: usize = 8 + 16 + 12;
const CHUNK: usize = 1 << 20;
const TAG: usize = 16;
/// Argon2id at OWASP's first recommendation: about half a second here (tests: cheap).
const MEMORY_KIB: u32 = if cfg!(test) { 64 } else { 64 * 1024 };
const PASSES: u32 = if cfg!(test) { 1 } else { 3 };
const LANES: u32 = 1;
/// A backup asking for more memory than this to open is not one of ours.
const MAX_MEMORY_KIB: u32 = 1024 * 1024;

pub(crate) const MANIFEST: &str = "manifest.json";
pub(crate) const DATABASE: &str = "projection.db";
pub(crate) const FILES: &str = "attachments";
pub(crate) const MIN_PASSPHRASE: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq)]
pub(crate) enum BackupError {
    #[error("this isn't a Hush backup")]
    NotABackup,
    #[error("the passphrase is wrong, or the backup is damaged")]
    Undecryptable,
    #[error("the backup is damaged: {0}")]
    Damaged(String),
    #[error("{0}")]
    Io(String),
}

/// A short read is a cut-off file (`cut`); any other read error is the disk's, as it is.
fn reading(e: std::io::Error, cut: BackupError) -> BackupError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        cut
    } else {
        e.into()
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// The key, or None if the parameters aren't usable.
fn key(passphrase: &str, salt: &[u8], memory: u32, passes: u32, lanes: u32) -> Option<[u8; 32]> {
    let params = argon2::Params::new(memory, passes, lanes, Some(32)).ok()?;
    let mut key = [0; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .ok()?;
    Some(key)
}

fn nonce(n: u64, last: bool) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[..8].copy_from_slice(&n.to_be_bytes());
    nonce[8..].copy_from_slice(&u32::from(last).to_be_bytes());
    nonce
}

const UNDECRYPTABLE: &str = "a chunk doesn't decrypt";

/// Writes a backup to `out`: add entries, then `finish`.
pub(crate) struct Writer<W: Write> {
    out: W,
    cipher: Aes256Gcm,
    header: [u8; HEADER_LEN],
    chunk: Vec<u8>,
    n: u64,
}

impl<W: Write> Writer<W> {
    pub(crate) fn new(mut out: W, passphrase: &str) -> Result<Self, BackupError> {
        let salt: [u8; 16] = rand::random();
        let mut header = [0; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8..24].copy_from_slice(&salt);
        header[24..28].copy_from_slice(&MEMORY_KIB.to_le_bytes());
        header[28..32].copy_from_slice(&PASSES.to_le_bytes());
        header[32..36].copy_from_slice(&LANES.to_le_bytes());
        let key = key(passphrase, &salt, MEMORY_KIB, PASSES, LANES).expect("our parameters are valid");
        let cipher = Aes256Gcm::new((&key).into());
        out.write_all(&header)?;
        Ok(Self {
            out,
            cipher,
            header,
            chunk: Vec::with_capacity(CHUNK),
            n: 0,
        })
    }

    fn put(&mut self, mut bytes: &[u8]) -> Result<(), BackupError> {
        while !bytes.is_empty() {
            if self.chunk.len() == CHUNK {
                self.seal(false)?;
            }
            let take = bytes.len().min(CHUNK - self.chunk.len());
            self.chunk.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn seal(&mut self, last: bool) -> Result<(), BackupError> {
        let sealed = self
            .cipher
            .encrypt(
                (&nonce(self.n, last)).into(),
                Payload {
                    msg: &self.chunk,
                    aad: &self.header,
                },
            )
            .expect("AES-GCM encryption doesn't fail");
        self.out.write_all(&[u8::from(last)])?;
        self.out.write_all(&(sealed.len() as u32).to_le_bytes())?;
        self.out.write_all(&sealed)?;
        self.chunk.clear();
        self.n += 1;
        Ok(())
    }

    fn entry_header(&mut self, name: &str, size: u64) -> Result<(), BackupError> {
        let len = u16::try_from(name.len()).map_err(|_| BackupError::Io(format!("name too long: {name}")))?;
        assert!(len > 0, "an entry has a name");
        self.put(&len.to_le_bytes())?;
        self.put(name.as_bytes())?;
        self.put(&size.to_le_bytes())
    }

    pub(crate) fn add_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), BackupError> {
        self.entry_header(name, bytes.len() as u64)?;
        self.put(bytes)
    }

    /// Adds `file` as `name`. Errors if it changes size meanwhile.
    pub(crate) fn add_file(&mut self, name: &str, mut file: std::fs::File) -> Result<(), BackupError> {
        let size = file.metadata()?.len();
        self.entry_header(name, size)?;
        let mut left = size;
        let mut buf = vec![0; 64 * 1024];
        while left > 0 {
            let want = buf.len().min(left as usize);
            let got = file.read(&mut buf[..want])?;
            if got == 0 {
                return Err(BackupError::Io(format!("{name} got shorter while it was backed up")));
            }
            self.put(&buf[..got])?;
            left -= got as u64;
        }
        Ok(())
    }

    /// Ends the backup and returns the output, flushed.
    pub(crate) fn finish(mut self) -> Result<W, BackupError> {
        self.put(&0u16.to_le_bytes())?;
        self.seal(true)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// The decrypted plaintext of a backup, read chunk by chunk.
struct Plain<R: Read> {
    input: R,
    cipher: Aes256Gcm,
    header: [u8; HEADER_LEN],
    chunk: Vec<u8>,
    at: usize,
    n: u64,
    ended: bool,
}

impl<R: Read> Plain<R> {
    fn open(mut input: R, passphrase: &str) -> Result<Self, BackupError> {
        let mut header = [0; HEADER_LEN];
        input
            .read_exact(&mut header)
            .map_err(|e| reading(e, BackupError::NotABackup))?;
        if &header[..8] != MAGIC {
            return Err(BackupError::NotABackup);
        }
        let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().expect("4 bytes"));
        let (memory, passes, lanes) = (u32_at(24), u32_at(28), u32_at(32));
        if memory > MAX_MEMORY_KIB || passes > 16 || lanes > 16 {
            return Err(BackupError::NotABackup);
        }
        let key = key(passphrase, &header[8..24], memory, passes, lanes).ok_or(BackupError::NotABackup)?;
        let cipher = Aes256Gcm::new((&key).into());
        let mut plain = Self {
            input,
            cipher,
            header,
            chunk: vec![],
            at: 0,
            n: 0,
            ended: false,
        };
        // the first chunk that doesn't decrypt is a wrong passphrase (or damage right there)
        plain.next_chunk().map_err(|e| match e {
            BackupError::Damaged(why) if why == UNDECRYPTABLE => BackupError::Undecryptable,
            e => e,
        })?;
        Ok(plain)
    }

    fn next_chunk(&mut self) -> Result<(), BackupError> {
        let cut = |e| reading(e, BackupError::Damaged("it ends too early".into()));
        let mut head = [0; 5];
        self.input.read_exact(&mut head).map_err(cut)?;
        let last = match head[0] {
            0 => false,
            1 => true,
            _ => return Err(BackupError::Damaged("a chunk is malformed".into())),
        };
        let len = u32::from_le_bytes(head[1..].try_into().expect("4 bytes")) as usize;
        if !(TAG..=CHUNK + TAG).contains(&len) {
            return Err(BackupError::Damaged("a chunk is malformed".into()));
        }
        let mut sealed = vec![0; len];
        self.input.read_exact(&mut sealed).map_err(cut)?;
        self.chunk = self
            .cipher
            .decrypt(
                (&nonce(self.n, last)).into(),
                Payload {
                    msg: &sealed,
                    aad: &self.header,
                },
            )
            .map_err(|_| BackupError::Damaged(UNDECRYPTABLE.into()))?;
        self.at = 0;
        self.n += 1;
        self.ended = last;
        if last && self.input.read(&mut [0])? != 0 {
            return Err(BackupError::Damaged("there is more after its end".into()));
        }
        Ok(())
    }

    fn read_exact(&mut self, mut out: &mut [u8]) -> Result<(), BackupError> {
        while !out.is_empty() {
            if self.at == self.chunk.len() {
                if self.ended {
                    return Err(BackupError::Damaged("an entry is cut off".into()));
                }
                self.next_chunk()?;
                continue;
            }
            let take = out.len().min(self.chunk.len() - self.at);
            out[..take].copy_from_slice(&self.chunk[self.at..self.at + take]);
            self.at += take;
            out = &mut out[take..];
        }
        Ok(())
    }
}

/// Where an entry goes under the restore folder: only the names a backup holds, so a crafted
/// one can't write elsewhere.
fn place(name: &str) -> Option<std::path::PathBuf> {
    match name.split_once('/') {
        None if name == MANIFEST || name == DATABASE => Some(name.into()),
        Some((FILES, file)) if !file.is_empty() && !file.contains('/') && !file.starts_with('.') => {
            Some(Path::new(FILES).join(file))
        }
        _ => None,
    }
}

/// Decrypts the backup in `input` into `dest` (created) and returns its manifest, which stays
/// in memory. Errors leave a partial `dest` for the caller to remove.
pub(crate) fn extract(input: impl Read, passphrase: &str, dest: &Path) -> Result<Vec<u8>, BackupError> {
    let mut plain = Plain::open(input, passphrase)?;
    std::fs::create_dir_all(dest.join(FILES))?;
    let mut manifest = None;
    loop {
        let mut len = [0; 2];
        plain.read_exact(&mut len)?;
        let len = u16::from_le_bytes(len) as usize;
        if len == 0 {
            break;
        }
        let mut name = vec![0; len];
        plain.read_exact(&mut name)?;
        let name = String::from_utf8(name).map_err(|_| BackupError::Damaged("an entry's name is malformed".into()))?;
        let path = place(&name).ok_or_else(|| BackupError::Damaged(format!("it holds an unexpected {name}")))?;
        if manifest.is_none() != (name == MANIFEST) {
            return Err(BackupError::Damaged("its manifest isn't first".into()));
        }
        let mut size = [0; 8];
        plain.read_exact(&mut size)?;
        let mut left = u64::from_le_bytes(size);
        if manifest.is_none() {
            let mut bytes = vec![
                0;
                usize::try_from(left)
                    .ok()
                    .filter(|n| *n <= 64 * 1024)
                    .ok_or_else(|| BackupError::Damaged("its manifest is too big".into()))?
            ];
            plain.read_exact(&mut bytes)?;
            manifest = Some(bytes);
            continue;
        }
        let mut file = std::fs::File::create(dest.join(path))?;
        let mut buf = vec![0; 64 * 1024];
        while left > 0 {
            let take = buf.len().min(left as usize);
            plain.read_exact(&mut buf[..take])?;
            file.write_all(&buf[..take])?;
            left -= take as u64;
        }
    }
    let manifest = manifest.ok_or_else(|| BackupError::Damaged("it is empty".into()))?;
    if plain.at != plain.chunk.len() || !plain.ended {
        return Err(BackupError::Damaged("there is more after its end".into()));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder; tests run in parallel, so each call gets its own.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hush-backup-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A backup with a manifest, a database bigger than a chunk (so several chunks) and a file.
    fn sample(passphrase: &str) -> Vec<u8> {
        let dir = temp_dir("sample-src");
        let big: Vec<u8> = (0..CHUNK * 2 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("photo"), b"a photo").unwrap();
        let mut w = Writer::new(Vec::new(), passphrase).unwrap();
        w.add_bytes(MANIFEST, b"{}").unwrap();
        w.add_bytes(DATABASE, &big).unwrap();
        w.add_file("attachments/7", std::fs::File::open(dir.join("photo")).unwrap())
            .unwrap();
        w.finish().unwrap()
    }

    #[test]
    fn a_backup_restores_what_went_in() {
        let dest = temp_dir("roundtrip");
        let manifest = extract(sample("correct horse").as_slice(), "correct horse", &dest).unwrap();
        assert_eq!(manifest, b"{}");
        assert!(!dest.join(MANIFEST).exists(), "the database's key isn't written out");
        let db = std::fs::read(dest.join(DATABASE)).unwrap();
        assert_eq!(db.len(), CHUNK * 2 + 17);
        assert!(db.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        assert_eq!(std::fs::read(dest.join("attachments/7")).unwrap(), b"a photo");
    }

    #[test]
    fn nothing_readable_is_written_in_clear() {
        let bytes = sample("correct horse");
        assert!(!bytes.windows(7).any(|w| w == b"a photo"));
        assert!(!bytes.windows(DATABASE.len()).any(|w| w == DATABASE.as_bytes()));
    }

    #[test]
    fn a_wrong_passphrase_is_told_apart() {
        let dest = temp_dir("wrong");
        let e = extract(sample("correct horse").as_slice(), "battery staple", &dest);
        assert_eq!(e, Err(BackupError::Undecryptable));
    }

    #[test]
    fn another_file_isnt_a_backup() {
        let dest = temp_dir("other");
        assert_eq!(
            extract(&b"PK\x03\x04 a zip file"[..], "x", &dest),
            Err(BackupError::NotABackup)
        );
        assert_eq!(extract(&b""[..], "x", &dest), Err(BackupError::NotABackup));
    }

    #[test]
    fn a_cut_off_backup_fails() {
        let bytes = sample("correct horse");
        // at a chunk boundary (only whole chunks left) and inside one
        let first_two = HEADER_LEN + 2 * (5 + CHUNK + TAG);
        for cut in [first_two, bytes.len() - 1] {
            let dest = temp_dir("cut");
            let e = extract(&bytes[..cut], "correct horse", &dest);
            assert!(matches!(e, Err(BackupError::Damaged(_))), "{cut}: {e:?}");
        }
    }

    #[test]
    fn every_size_around_a_chunk_restores() {
        // 77 bytes besides the database's: the end marker split across chunks or not, the
        // first chunk exactly full, an empty file
        for size in [CHUNK - 79, CHUNK - 78, CHUNK - 77, CHUNK - 76, CHUNK - 75, CHUNK] {
            let mut w = Writer::new(Vec::new(), "correct horse").unwrap();
            w.add_bytes(MANIFEST, b"{}").unwrap();
            w.add_bytes("attachments/empty", b"").unwrap();
            w.add_bytes(DATABASE, &vec![7; size]).unwrap();
            let dest = temp_dir("sizes");
            extract(w.finish().unwrap().as_slice(), "correct horse", &dest).unwrap_or_else(|e| panic!("{size}: {e}"));
            assert_eq!(std::fs::read(dest.join(DATABASE)).unwrap().len(), size);
            assert!(std::fs::read(dest.join("attachments/empty")).unwrap().is_empty());
        }
    }

    #[test]
    fn a_chunk_out_of_place_fails() {
        let bytes = sample("correct horse");
        let len = 5 + CHUNK + TAG;
        let (first, second) = (HEADER_LEN, HEADER_LEN + len);
        let mut swapped = bytes.clone();
        swapped[second..second + len].copy_from_slice(&bytes[first..first + len]);
        let dest = temp_dir("swap");
        assert!(matches!(
            extract(swapped.as_slice(), "correct horse", &dest),
            Err(BackupError::Damaged(_))
        ));
        // the second chunk passed off as the last
        let mut early = bytes[..second + len].to_vec();
        early[second] = 1;
        let dest = temp_dir("early");
        assert!(matches!(
            extract(early.as_slice(), "correct horse", &dest),
            Err(BackupError::Damaged(_))
        ));
    }

    #[test]
    fn a_file_cut_after_its_header_is_damaged_not_a_wrong_passphrase() {
        let bytes = sample("correct horse");
        let dest = temp_dir("header-only");
        let e = extract(&bytes[..HEADER_LEN + 3], "correct horse", &dest);
        assert!(matches!(e, Err(BackupError::Damaged(_))), "{e:?}");
    }

    #[test]
    fn a_changed_byte_fails() {
        let mut bytes = sample("correct horse");
        let at = bytes.len() - 40;
        bytes[at] ^= 1;
        let dest = temp_dir("flip");
        assert!(matches!(
            extract(bytes.as_slice(), "correct horse", &dest),
            Err(BackupError::Damaged(_))
        ));
    }

    #[test]
    fn trailing_bytes_fail() {
        let mut bytes = sample("correct horse");
        bytes.push(0);
        let dest = temp_dir("trailing");
        assert!(matches!(
            extract(bytes.as_slice(), "correct horse", &dest),
            Err(BackupError::Damaged(_))
        ));
    }

    #[test]
    fn entries_outside_the_restore_folder_are_refused() {
        for name in [
            "../escape",
            "attachments/../../x",
            "attachments/",
            "attachments/.hidden",
            "/etc/x",
            "other.db",
        ] {
            let mut w = Writer::new(Vec::new(), "correct horse").unwrap();
            w.add_bytes(MANIFEST, b"{}").unwrap();
            w.add_bytes(name, b"x").unwrap();
            let bytes = w.finish().unwrap();
            let dest = temp_dir("escape");
            let e = extract(bytes.as_slice(), "correct horse", &dest);
            assert!(matches!(e, Err(BackupError::Damaged(_))), "{name}: {e:?}");
        }
    }

    #[test]
    fn the_manifest_comes_first() {
        let mut w = Writer::new(Vec::new(), "correct horse").unwrap();
        w.add_bytes(DATABASE, b"db").unwrap();
        w.add_bytes(MANIFEST, b"{}").unwrap();
        let bytes = w.finish().unwrap();
        let dest = temp_dir("order");
        assert!(matches!(
            extract(bytes.as_slice(), "correct horse", &dest),
            Err(BackupError::Damaged(_))
        ));
    }

    #[test]
    fn absurd_key_parameters_are_refused_before_hashing() {
        let mut bytes = sample("correct horse");
        bytes[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        let dest = temp_dir("params");
        assert_eq!(
            extract(bytes.as_slice(), "correct horse", &dest),
            Err(BackupError::NotABackup)
        );
    }
}
