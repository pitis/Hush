//! A chat's files: pictures inline, recordings played in their bubble, any other file as a row
//! that opens in the system's app for it, and each can be saved (the Mac app's
//! `AttachmentViews.swift`, `Files.swift`). Files are encrypted on disk: the core decrypts them, in
//! memory (`file_bytes`) or into a copy (`copy_file`); they are never read here directly.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_compat::Compat;
use gpui_kit::*;
use image::ImageDecoder as _;
use rodio::Source as _;
use sig_core::projection::{Attachment, AttachmentState, FileSummary, Message, MessageKind};
use sig_core::Client;

use super::Hush;

mod view;

pub(super) use view::render;

/// An image bubble's longest side at most (`Files.imageBox`).
const IMAGE_BOX: f64 = 320.;
/// Pictures are decoded at most this many pixels either way: the box at 2× (`Thumbnails.maxPixels`).
const MAX_PIXELS: u32 = 640;
/// A larger recording isn't read whole just for its length: it shows once played
/// (`AudioAttachmentView.lengthReadLimit`).
const LENGTH_READ_LIMIT: u64 = 20 << 20;
/// Opened files' decrypted copies, in the data folder (which only we can read).
const OPEN_FOLDER: &str = "open";

/// The open chat's files, and what is shown of them.
#[derive(Default)]
pub(super) struct Files {
    /// The open chat's messages' files, by message (author, sent at).
    by_message: HashMap<(String, i64), Vec<Attachment>>,
    loading: bool,
    again: bool,
    /// Pictures by their file, decoded small.
    /// shortcut: kept for the open chat only (read again when it is opened again); the Mac app
    /// keeps 32 MB of them across chats.
    thumbs: HashMap<String, Thumb>,
    /// Recordings' lengths by file; None: unknown until played.
    lengths: HashMap<i64, Option<Duration>>,
    unplayable: HashSet<i64>,
    /// One recording plays at a time.
    playing: Option<Playing>,
    /// A recording being read to play: another click waits for it.
    opening: Option<i64>,
    /// Plays so far, so a play's clock stops with it.
    plays: u64,
}

enum Thumb {
    Loading,
    Shown(Arc<RenderImage>),
    /// Read, but not a picture this can decode.
    Undecodable,
}

struct Playing {
    id: i64,
    play: u64,
    player: rodio::Player,
    /// The sound stops when this goes.
    _device: rodio::MixerDeviceSink,
}

/// A file as its message shows it.
pub(super) struct Shown {
    pub file: Attachment,
    pub look: Look,
}

pub(super) enum Look {
    /// Its picture, once decoded.
    Picture(Option<Arc<RenderImage>>),
    Audio(Audio),
    File,
}

pub(super) struct Audio {
    pub playing: bool,
    /// Seconds played, and how long it is (0: not known yet).
    pub elapsed: f64,
    pub length: f64,
    pub unplayable: bool,
}

impl Files {
    /// A message's files as shown; none for a deleted message, a view-once one or a sticker
    /// (those show otherwise).
    pub(super) fn of(&self, m: &Message) -> Vec<Shown> {
        if m.deleted || matches!(m.kind, MessageKind::ViewOnce { .. } | MessageKind::Sticker { .. }) {
            return vec![];
        }
        let Some(files) = self.by_message.get(&(m.author.clone(), m.sent_at)) else {
            return vec![];
        };
        let shown = files.iter().map(|a| Shown {
            look: self.look(a),
            file: a.clone(),
        });
        shown.collect()
    }

    fn look(&self, a: &Attachment) -> Look {
        if is_image(a) {
            return match ready_path(a).and_then(|path| self.thumbs.get(&path)) {
                // one that says it is a picture but isn't: still there to open
                Some(Thumb::Undecodable) => Look::File,
                Some(Thumb::Shown(thumb)) => Look::Picture(Some(thumb.clone())),
                _ => Look::Picture(None),
            };
        }
        if is_audio(a) {
            let playing = self.playing.as_ref().filter(|p| p.id == a.id);
            return Look::Audio(Audio {
                playing: playing.is_some_and(|p| !p.player.is_paused()),
                elapsed: playing.map_or(0., |p| p.player.get_pos().as_secs_f64()),
                length: self
                    .lengths
                    .get(&a.id)
                    .copied()
                    .flatten()
                    .map_or(0., |d| d.as_secs_f64()),
                unplayable: self.unplayable.contains(&a.id),
            });
        }
        Look::File
    }

    /// Made-up files for `--snapshot`: their pictures are drawn here, not read.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo(files: Vec<Attachment>) -> Self {
        let mut shown = Self::default();
        for a in files {
            if let (true, Some(path)) = (is_image(&a), ready_path(&a)) {
                let picture = thumbnail(&crate::demo::picture()).expect("the made-up picture decodes");
                shown.thumbs.insert(path, Thumb::Shown(Arc::new(picture)));
            }
            if is_audio(&a) {
                shown.lengths.insert(a.id, Some(Duration::from_secs(42)));
            }
            let key = (a.message.author.clone(), a.message.sent_at);
            shown.by_message.entry(key).or_default().push(a);
        }
        shown
    }
}

impl Hush {
    /// Reads the open chat's messages' files; calls while a read runs are folded into one more.
    pub(super) fn load_files(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.files.loading {
            self.files.again = true;
            return;
        }
        self.files.loading = true;
        cx.spawn(async move |this, cx| loop {
            let page = this.update(cx, |app, _| {
                let keys: Vec<_> = app.messages.iter().map(Message::key).collect();
                (app.generation, keys)
            });
            let Ok((generation, keys)) = page else { return };
            let found = Compat::new(async {
                let mut found = HashMap::new();
                for key in keys {
                    let files = client.attachments(key.clone()).await?;
                    if !files.is_empty() {
                        found.insert((key.author, key.sent_at), files);
                    }
                }
                Ok::<_, sig_core::CoreError>(found)
            })
            .await;
            let again = this.update(cx, |app, cx| {
                // another chat was opened meanwhile: its messages read its files
                if app.generation == generation {
                    match found {
                        Ok(found) => {
                            app.files.by_message = found;
                            app.load_media(cx);
                        }
                        Err(e) => app.error = Some(format!("Couldn't load the files: {e}")),
                    }
                }
                app.files.loading = std::mem::take(&mut app.files.again);
                cx.notify();
                app.files.loading
            });
            if !again.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    /// A file was added or changed state (downloading, ready, failed).
    pub(super) fn attachment_changed(&mut self, conversation_id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(conversation_id) {
            self.load_files(cx);
        }
    }

    /// Decodes the pictures and reads the recordings' lengths not asked for yet, one at a time.
    fn load_media(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let (mut pictures, mut recordings) = (vec![], vec![]);
        for a in self.files.by_message.values().flatten() {
            let Some(path) = ready_path(a) else { continue };
            if is_image(a) && !self.files.thumbs.contains_key(&path) {
                self.files.thumbs.insert(path.clone(), Thumb::Loading);
                pictures.push(path);
            } else if is_audio(a) && a.size <= LENGTH_READ_LIMIT && !self.files.lengths.contains_key(&a.id) {
                self.files.lengths.insert(a.id, None);
                recordings.push((a.id, path, a.content_type.clone()));
            }
        }
        if pictures.is_empty() && recordings.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for path in pictures {
                // decrypted in memory only
                let decoded = match Compat::new(client.file_bytes(path.clone())).await {
                    Ok(bytes) => Some(cx.background_spawn(async move { thumbnail(&bytes) }).await),
                    Err(_) => None,
                };
                let shown = this.update(cx, |app, cx| {
                    // gone with the chat it was in
                    if !matches!(app.files.thumbs.get(&path), Some(Thumb::Loading)) {
                        return;
                    }
                    match decoded {
                        Some(Some(thumb)) => app.files.thumbs.insert(path, Thumb::Shown(Arc::new(thumb))),
                        Some(None) => app.files.thumbs.insert(path, Thumb::Undecodable),
                        // the core logs why; read again with the next change
                        None => app.files.thumbs.remove(&path),
                    };
                    cx.notify();
                });
                if shown.is_err() {
                    return;
                }
            }
            for (id, path, content_type) in recordings {
                // unknown until played
                let Ok(bytes) = Compat::new(client.file_bytes(path)).await else {
                    continue;
                };
                let found = cx.background_spawn(async move { length(bytes, &content_type) }).await;
                let shown = this.update(cx, |app, cx| {
                    if found.is_some() {
                        app.files.lengths.insert(id, found);
                        cx.notify();
                    }
                });
                if shown.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Leaving a chat: its pictures' memory goes and its recording stops.
    pub(super) fn clear_files(&mut self, cx: &mut Context<Self>) {
        self.files.by_message.clear();
        let shown: Vec<_> = self.files.thumbs.drain().collect();
        // once no window is being drawn, so each lets their copy go
        cx.defer(move |cx| {
            for (_, thumb) in shown {
                if let Thumb::Shown(thumb) = thumb {
                    cx.drop_image(thumb, None);
                }
            }
        });
        self.files.playing = None;
        self.files.opening = None;
    }

    /// Downloads a file that didn't download by itself, or failed.
    pub(super) fn download(&mut self, a: &Attachment, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        if let Err(e) = client.download_attachment(a.id) {
            self.error = Some(format!("Couldn't download the file: {e}"));
            cx.notify();
        }
    }

    /// Opens a downloaded file in the system's app for it (the Mac app's Quick Look), from a
    /// decrypted copy under its own name in a folder of its own; copies go at quit.
    pub(super) fn open_file(&mut self, a: &Attachment, cx: &mut Context<Self>) {
        let (Some(client), Some(path)) = (self.client.clone(), ready_path(a)) else {
            return;
        };
        let dir = self
            .data_dir
            .join(OPEN_FOLDER)
            .join(hex::encode(rand::random::<[u8; 8]>()));
        let to = dir.join(file_name(a));
        let what = a.file_name.clone().unwrap_or_else(|| "the file".into());
        cx.spawn(async move |this, cx| {
            let made = match std::fs::create_dir_all(&dir) {
                Ok(()) => copy_out(&client, path, &to, true).await,
                Err(e) => Err(e.to_string()),
            };
            let _ = this.update(cx, |app, cx| match made {
                Ok(()) => cx.open_with_system(&to),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    app.error = Some(format!("Couldn't open {what}: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Saves a decrypted copy of a downloaded file where the user picks.
    pub(super) fn save_file(&mut self, a: &Attachment, cx: &mut Context<Self>) {
        let (Some(client), Some(path)) = (self.client.clone(), ready_path(a)) else {
            return;
        };
        let name = file_name(a);
        let folder = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default();
        let chosen = cx.prompt_for_new_path(&folder, Some(&name));
        cx.spawn(async move |this, cx| {
            let saved = match chosen.await {
                Ok(Ok(Some(to))) => save(&client, path, &to).await,
                Ok(Err(e)) => Err(e.to_string()),
                // cancelled
                Ok(Ok(None)) | Err(_) => Ok(()),
            };
            if let Err(e) = saved {
                let _ = this.update(cx, |app, cx| {
                    app.error = Some(format!("Couldn't save {name}: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Plays or pauses a recording, read into memory (never a decrypted copy on disk); one
    /// playing before stops.
    pub(super) fn toggle_play(&mut self, a: &Attachment, cx: &mut Context<Self>) {
        if self.files.opening.is_some() {
            return;
        }
        if let Some(p) = self.files.playing.as_ref().filter(|p| p.id == a.id) {
            if p.player.is_paused() {
                p.player.play();
            } else {
                p.player.pause();
            }
            return cx.notify();
        }
        let (Some(client), Some(path)) = (self.client.clone(), ready_path(a)) else {
            return;
        };
        self.files.playing = None;
        self.files.opening = Some(a.id);
        let (id, content_type) = (a.id, a.content_type.clone());
        cx.spawn(async move |this, cx| {
            let decoded = match Compat::new(client.file_bytes(path)).await {
                Ok(bytes) => {
                    cx.background_spawn(async move { decoder(bytes, &content_type).ok() })
                        .await
                }
                Err(_) => None,
            };
            let _ = this.update(cx, |app, cx| {
                // the chat was left meanwhile
                if app.files.opening != Some(id) {
                    return;
                }
                app.files.opening = None;
                match decoded {
                    Some(source) => app.play(id, source, cx),
                    None => {
                        app.files.unplayable.insert(id);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn play(&mut self, id: i64, source: rodio::Decoder<Cursor<Vec<u8>>>, cx: &mut Context<Self>) {
        if let Some(length) = source.total_duration() {
            self.files.lengths.insert(id, Some(length));
        }
        let mut device = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(device) => device,
            Err(e) => {
                self.error = Some(format!("Couldn't play the recording: {e}"));
                return;
            }
        };
        device.log_on_drop(false);
        let player = rodio::Player::connect_new(device.mixer());
        player.append(source);
        self.files.plays += 1;
        let play = self.files.plays;
        self.files.playing = Some(Playing {
            id,
            play,
            player,
            _device: device,
        });
        // its time moves on while it plays; at its end it is ready to play again from the start
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let going = this.update(cx, |app, cx| {
                let Some(p) = app.files.playing.as_ref().filter(|p| p.play == play) else {
                    return false;
                };
                if p.player.empty() {
                    app.files.playing = None;
                    cx.notify();
                    return false;
                }
                if !p.player.is_paused() {
                    cx.notify();
                }
                true
            });
            if !going.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }
}

/// Copies a previous run left open, or this one: they go, at launch (once the core holds the
/// data folder) and at quit.
pub(super) fn clear_open_copies(data_dir: &Path) {
    // one still open elsewhere (Windows keeps it) goes at the next launch
    let _ = std::fs::remove_dir_all(data_dir.join(OPEN_FOLDER));
}

/// `path`'s file decrypted into a new file at `to`, marked as come from someone else. `marked`:
/// a copy that can't carry the mark goes.
async fn copy_out(client: &Client, path: String, to: &Path, marked: bool) -> Result<(), String> {
    let at = to.to_str().ok_or("the folder's path isn't valid text")?.to_owned();
    Compat::new(client.copy_file(path, at))
        .await
        .map_err(|e| e.to_string())?;
    match mark_from_elsewhere(to) {
        Err(e) if marked => {
            let _ = std::fs::remove_file(to);
            Err(format!("couldn't mark its copy: {e}"))
        }
        _ => Ok(()),
    }
}

/// Written beside `to` first, so a file there is replaced only by a whole copy. A drive that
/// can't hold the mark (FAT) gets the copy without it, as Windows' own copies there.
async fn save(client: &Client, path: String, to: &Path) -> Result<(), String> {
    let part = to.with_file_name(format!(".{}.part", hex::encode(rand::random::<[u8; 8]>())));
    copy_out(client, path, &part, false).await?;
    std::fs::rename(&part, to).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        e.to_string()
    })
}

fn ready_path(a: &Attachment) -> Option<String> {
    a.path.clone().filter(|_| a.state == AttachmentState::Ready)
}

pub(super) fn summary(a: &Attachment) -> FileSummary {
    FileSummary {
        content_type: a.content_type.clone(),
        file_name: a.file_name.clone(),
        is_voice_note: a.is_voice_note,
    }
}

pub fn is_image(a: &Attachment) -> bool {
    a.content_type.starts_with("image/") && !a.is_voice_note
}

pub fn is_audio(a: &Attachment) -> bool {
    a.is_voice_note || a.content_type.starts_with("audio/")
}

/// An image's size in a bubble: its shape, at most `IMAGE_BOX` either way, never enlarged.
fn display_size(width: Option<u32>, height: Option<u32>) -> (f32, f32) {
    let (Some(w), Some(h)) = (width.filter(|&w| w > 0), height.filter(|&h| h > 0)) else {
        return (240., 180.);
    };
    let (w, h) = (f64::from(w), f64::from(h));
    let scale = (IMAGE_BOX / w).min(IMAGE_BOX / h).min(1.);
    ((w * scale).floor() as f32, (h * scale).floor() as f32)
}

/// A file's size as `ByteCountFormatter` (`.file`) writes it: whole KB, MB to a tenth, GB to a
/// hundredth.
fn byte_count(bytes: u64) -> String {
    match bytes {
        0 => return "Zero KB".into(),
        1 => return "1 byte".into(),
        n if n < 1_000 => return format!("{n} bytes"),
        _ => {}
    }
    let units = [("KB", 1e3, 0), ("MB", 1e6, 1), ("GB", 1e9, 2)];
    let (unit, value) = units
        .iter()
        .map(|&(unit, size, places)| (unit, format!("{:.*}", places, bytes as f64 / size)))
        .find(|(unit, value)| *unit == "GB" || value.parse::<f64>().is_ok_and(|v| v < 1000.))
        .expect("GB takes the rest");
    let value = if value.contains('.') {
        value.trim_end_matches('0').trim_end_matches('.')
    } else {
        &value
    };
    format!("{value} {unit}")
}

/// A recording's time: "0:07", "12:34".
fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0. {
        return "0:00".into();
    }
    let s = seconds.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The name a file's copy gets: the sender's, only its last part and without what a name can't
/// hold on Windows; or "file" with its type's extension.
fn file_name(a: &Attachment) -> String {
    let given = a.file_name.as_deref().and_then(|n| n.rsplit(['/', '\\']).next());
    let given = given.map(|n| {
        let n: String = n
            .chars()
            .map(|c| {
                if c.is_control() || "<>:\"|?*".contains(c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        // Windows drops a name's trailing dots and spaces (and with them "." and "..")
        n.trim_end_matches(['.', ' ']).to_string()
    });
    let name = given
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| match extension(&a.content_type) {
            Some(ext) => format!("file.{ext}"),
            None => "file".into(),
        });
    // a Windows device's name, with an extension too
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let numbered =
        stem.len() == 4 && ["COM", "LPT"].iter().any(|p| stem.starts_with(p)) && stem.as_bytes()[3].is_ascii_digit();
    if numbered || ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) {
        format!("_{name}")
    } else {
        name
    }
}

/// A file type's usual extension, for a copy without a name.
fn extension(content_type: &str) -> Option<&str> {
    let t = content_type.split(';').next().unwrap_or_default().trim();
    Some(match t {
        "image/jpeg" => "jpg",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "text/plain" => "txt",
        _ => t
            .split_once('/')
            .map(|(_, subtype)| subtype)
            .filter(|s| (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))?,
    })
}

/// A picture decoded at most `MAX_PIXELS` either way, turned as its photo says, in GPUI's BGRA;
/// None for one this can't decode.
fn thumbnail(bytes: &[u8]) -> Option<RenderImage> {
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut picture = image::DynamicImage::from_decoder(decoder).ok()?;
    picture.apply_orientation(orientation);
    if picture.width() > MAX_PIXELS || picture.height() > MAX_PIXELS {
        picture = picture.thumbnail(MAX_PIXELS, MAX_PIXELS);
    }
    let mut pixels = picture.into_rgba8();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(RenderImage::new([image::Frame::new(pixels)]))
}

fn decoder(
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, rodio::decoder::DecoderError> {
    let len = bytes.len() as u64;
    rodio::Decoder::builder()
        .with_data(Cursor::new(bytes))
        .with_byte_len(len)
        .with_mime_type(content_type)
        .build()
}

/// A recording's length from its bytes, where its format says it.
fn length(bytes: Vec<u8>, content_type: &str) -> Option<Duration> {
    decoder(bytes, content_type)
        .ok()?
        .total_duration()
        .filter(|d| !d.is_zero())
}

/// Marks a copy as come from someone else, as browsers mark downloads, so the system checks it
/// before it runs (a script or a program a contact sent): Gatekeeper's quarantine on macOS.
#[cfg(target_os = "macos")]
fn mark_from_elsewhere(file: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    // Launch Services' download flags, as the Mac app writes them
    let mark = format!("0081;{:x};Hush;", now.map_or(0, |d| d.as_secs()));
    let path = std::ffi::CString::new(file.as_os_str().as_bytes())?;
    // SAFETY: both names are NUL-terminated and `mark` is as long as said, for the call
    let set = unsafe {
        libc::setxattr(
            path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            mark.as_ptr().cast(),
            mark.len(),
            0,
            0,
        )
    };
    match set {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Windows: the Mark of the Web, the Internet zone's, which SmartScreen and Office check.
#[cfg(windows)]
fn mark_from_elsewhere(file: &Path) -> std::io::Result<()> {
    let mut stream = file.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n")
}

/// Linux has no such mark; a copy is never made executable.
#[cfg(not(any(target_os = "macos", windows)))]
fn mark_from_elsewhere(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    // not `super::*`: GPUI's `test` attribute would come with it
    use super::{
        byte_count, clear_open_copies, clock, display_size, file_name, is_audio, is_image, length, thumbnail,
        Attachment, Duration, Files, Look, Thumb,
    };
    use gpui_kit::{DevicePixels, Size};
    use sig_core::projection::{AttachmentState, Message, MessageKey, MessageKind, MessageStatus};
    use std::sync::Arc;

    fn file(content_type: &str, name: Option<&str>) -> Attachment {
        Attachment {
            id: 1,
            message: MessageKey {
                author: "a".into(),
                sent_at: 1,
            },
            content_type: content_type.into(),
            file_name: name.map(Into::into),
            size: 0,
            width: None,
            height: None,
            is_voice_note: false,
            path: None,
            state: AttachmentState::Ready,
        }
    }

    #[test]
    fn pictures_and_recordings_are_told_by_their_type() {
        assert!(is_image(&file("image/jpeg", None)));
        assert!(!is_image(&file("video/mp4", None)));
        assert!(is_audio(&file("audio/aac", None)));
        let mut voice = file("application/octet-stream", None);
        voice.is_voice_note = true;
        assert!(is_audio(&voice) && !is_image(&voice));
    }

    #[test]
    fn pictures_fit_the_box_and_are_never_enlarged() {
        assert_eq!(display_size(Some(1000), Some(500)), (320., 160.));
        assert_eq!(display_size(Some(500), Some(1000)), (160., 320.));
        assert_eq!(display_size(Some(100), Some(50)), (100., 50.));
        assert_eq!(display_size(Some(999), Some(1000)), (319., 320.), "rounded down");
        assert_eq!(display_size(None, Some(10)), (240., 180.), "size unknown");
        assert_eq!(display_size(Some(0), Some(10)), (240., 180.));
    }

    #[test]
    fn sizes_read_as_the_mac_app_writes_them() {
        assert_eq!(byte_count(0), "Zero KB");
        assert_eq!(byte_count(1), "1 byte");
        assert_eq!(byte_count(512), "512 bytes");
        assert_eq!(byte_count(340_000), "340 KB");
        assert_eq!(byte_count(1_234_567), "1.2 MB");
        assert_eq!(byte_count(2_000_000), "2 MB");
        assert_eq!(byte_count(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn a_recordings_time_is_minutes_and_seconds() {
        assert_eq!(clock(0.), "0:00");
        assert_eq!(clock(-1.), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
        assert_eq!(clock(7.4), "0:07");
        assert_eq!(clock(59.6), "1:00");
        assert_eq!(clock(754.), "12:34");
    }

    #[test]
    fn a_copys_name_stays_in_its_folder_and_has_an_extension() {
        assert_eq!(file_name(&file("image/jpeg", None)), "file.jpg");
        assert_eq!(file_name(&file("video/mp4", None)), "file.mp4");
        assert_eq!(file_name(&file("application/octet-stream", None)), "file");
        assert_eq!(file_name(&file("application/pdf", Some("Menu.pdf"))), "Menu.pdf");
        assert_eq!(file_name(&file("text/plain", Some("../../.ssh/config"))), "config");
        assert_eq!(file_name(&file("text/plain", Some("..\\..\\boot.ini"))), "boot.ini");
        assert_eq!(file_name(&file("text/plain", Some(".."))), "file.txt");
        assert_eq!(file_name(&file("text/plain", Some("a:b?.txt"))), "a_b_.txt");
        assert_eq!(file_name(&file("text/plain", Some("notes. . "))), "notes");
        assert_eq!(
            file_name(&file("text/plain", Some("con.txt"))),
            "_con.txt",
            "a Windows device"
        );
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut out = std::io::Cursor::new(vec![]);
        picture.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn pictures_are_decoded_small_in_gpuis_colour_order() {
        let big = thumbnail(&png(1280, 640)).unwrap();
        assert_eq!(big.size(0), size2(640, 320));
        // red as BGRA
        assert_eq!(&big.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        assert_eq!(
            thumbnail(&png(10, 20)).unwrap().size(0),
            size2(10, 20),
            "never enlarged"
        );
        assert!(thumbnail(b"HUSHFIL1 still encrypted").is_none());
    }

    fn size2(w: i32, h: i32) -> Size<DevicePixels> {
        gpui_kit::size(DevicePixels(w), DevicePixels(h))
    }

    /// A second of silence as a 16-bit mono WAV at 8 kHz.
    fn wav() -> Vec<u8> {
        let data = vec![0u8; 16_000];
        let mut out = b"RIFF".to_vec();
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes()); // PCM
        out.extend(1u16.to_le_bytes()); // mono
        out.extend(8_000u32.to_le_bytes());
        out.extend(16_000u32.to_le_bytes()); // bytes a second
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn a_recordings_length_is_read_from_its_bytes() {
        assert_eq!(length(wav(), "audio/wav"), Some(Duration::from_secs(1)));
        assert_eq!(length(b"not a recording".to_vec(), "audio/aac"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn copies_carry_the_quarantine_mark() {
        let dir = std::env::temp_dir().join(format!("hush-desktop-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("run me.command");
        std::fs::write(&copy, "echo hi").unwrap();
        super::mark_from_elsewhere(&copy).unwrap();
        let path = std::ffi::CString::new(copy.to_str().unwrap()).unwrap();
        let mut value = [0u8; 128];
        // SAFETY: both strings are NUL-terminated and `value` is as long as said
        let n = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(n > 0, "marked");
        assert!(value[..n as usize].starts_with(b"0081;"), "as downloaded");
    }

    fn message(kind: MessageKind) -> Message {
        Message {
            conversation_id: "c:a".into(),
            author: "a".into(),
            sent_at: 1,
            received_at: 1,
            body: None,
            ranges: vec![],
            is_outgoing: false,
            status: MessageStatus::Read,
            edited: false,
            deleted: false,
            quote: None,
            kind,
            expires_in: 0,
            expires_at: None,
            story: None,
        }
    }

    /// The files of `message`: each kind, in each state.
    fn shown() -> Files {
        let mut files = Files::default();
        let ready = |id: i64, content_type: &str| Attachment {
            id,
            path: Some(format!("f{id}")),
            ..file(content_type, None)
        };
        let decoded = ready(1, "image/jpeg");
        let broken = ready(2, "image/heic");
        let coming = Attachment {
            id: 3,
            state: AttachmentState::Pending,
            ..file("image/jpeg", None)
        };
        let voice = Attachment {
            is_voice_note: true,
            ..ready(4, "audio/aac")
        };
        let video = ready(5, "video/mp4");
        let picture = thumbnail(&png(4, 4)).unwrap();
        files.thumbs.insert("f1".into(), Thumb::Shown(Arc::new(picture)));
        files.thumbs.insert("f2".into(), Thumb::Undecodable);
        files.lengths.insert(4, Some(Duration::from_secs(42)));
        files
            .by_message
            .insert(("a".into(), 1), vec![decoded, broken, coming, voice, video]);
        files
    }

    #[test]
    fn each_file_shows_as_its_kind_and_state_say() {
        let looks: Vec<String> = shown()
            .of(&message(MessageKind::Normal))
            .iter()
            .map(|f| match &f.look {
                Look::Picture(Some(_)) => "picture".into(),
                Look::Picture(None) => "placeholder".into(),
                Look::Audio(a) => format!("audio {} {}", a.length, a.playing),
                Look::File => "file".into(),
            })
            .collect();
        // a picture that didn't decode is a file to open; a video opens in the system's player
        let expected = ["picture", "file", "placeholder", "audio 42 false", "file"];
        assert_eq!(looks, expected);
    }

    #[test]
    fn deleted_view_once_and_sticker_messages_show_no_files() {
        let files = shown();
        let mut deleted = message(MessageKind::Normal);
        deleted.deleted = true;
        assert!(files.of(&deleted).is_empty());
        assert!(files.of(&message(MessageKind::ViewOnce { viewed: false })).is_empty());
        assert!(files.of(&message(MessageKind::Sticker { emoji: None })).is_empty());
        let mut other = message(MessageKind::Normal);
        other.sent_at = 2;
        assert!(files.of(&other).is_empty(), "another message's");
    }

    #[test]
    fn opened_copies_go_and_the_rest_of_the_data_folder_stays() {
        let data = std::env::temp_dir().join(format!("hush-desktop-open-{}", std::process::id()));
        let copy = data.join("open").join("0123").join("Menu.pdf");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, "menu").unwrap();
        std::fs::write(data.join("projection.db"), "kept").unwrap();
        clear_open_copies(&data);
        let gone = !data.join("open").exists();
        let kept = data.join("projection.db").exists();
        std::fs::remove_dir_all(&data).unwrap();
        assert!(gone && kept);
        clear_open_copies(&data); // nothing left: no error
    }
}
