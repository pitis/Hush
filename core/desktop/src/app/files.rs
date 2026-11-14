//! A chat's files: pictures inline, recordings played in their bubble, any other file as a row
//! that opens in the system's app for it, and each can be saved (the Mac app's
//! `AttachmentViews.swift`, `Files.swift`). Files are encrypted on disk: the core decrypts them, in
//! memory (`file_bytes`) or into a copy (`copy_file`); they are never read here directly.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_compat::Compat;
use gpui_kit::*;
use image::ImageDecoder as _;
use rodio::Source as _;
use sig_core::projection::{
    Attachment, AttachmentState, FileSummary, Message, MessageDetails, MessageKey, MessageKind,
};
use sig_core::Client;

use super::Hush;

mod view;

pub(super) use view::render;

/// An image bubble's longest side at most (`Files.imageBox`).
const IMAGE_BOX: f64 = 320.;
/// Pictures are decoded at most this many pixels either way: the box at 2× (`Thumbnails.maxPixels`).
const MAX_PIXELS: u32 = 640;
/// A larger picture isn't decoded (its pixels alone would take 256 MB): it opens as a file.
const MAX_DECODED_PIXELS: u64 = 64_000_000;
/// A larger recording isn't read whole just for its length: it shows once played
/// (`AudioAttachmentView.lengthReadLimit`).
const LENGTH_READ_LIMIT: u64 = 20 << 20;
/// A recording plays from memory: a larger one opens in the system's player instead.
const PLAY_LIMIT: u64 = 50 << 20;
/// The track's width.
const TRACK: f64 = 140.;
/// A playing recording's clock moves this often.
const TICK: Duration = Duration::from_millis(100);
/// Not moving for this many ticks while playing: the sound output went away.
const STALL_TICKS: u32 = 30;
/// Paused this long, a recording lets the sound output go (and starts over when played again).
const IDLE_TICKS: u32 = 1_200;
/// Longer names are cut, keeping their extension.
const NAME_LIMIT: usize = 200;
/// Opened files' decrypted copies, in the data folder (which only we can read).
const OPEN_FOLDER: &str = "open";

/// Programs and scripts, which are only saved, never handed to the system to open: Signal
/// Desktop's list (`isFileDangerous`), and macOS's and Linux's.
const DANGEROUS: [&str; 72] = [
    "ade",
    "adp",
    "apk",
    "bat",
    "cab",
    "chm",
    "cmd",
    "com",
    "cpl",
    "diagcab",
    "dll",
    "dmg",
    "exe",
    "hta",
    "inf",
    "ins",
    "isp",
    "jar",
    "js",
    "jse",
    "lib",
    "lnk",
    "mde",
    "mht",
    "msc",
    "msi",
    "msp",
    "mst",
    "nsh",
    "pif",
    "ps1",
    "psc1",
    "psm1",
    "psrc",
    "reg",
    "scr",
    "sct",
    "settingcontent-ms",
    "shb",
    "sys",
    "vb",
    "vbe",
    "vbs",
    "vxd",
    "wsc",
    "wsf",
    "wsh",
    "url", // macOS
    "app",
    "command",
    "tool",
    "terminal",
    "workflow",
    "pkg",
    "mpkg",
    "fileloc",
    "inetloc", // Linux
    "sh",
    "bash",
    "zsh",
    "csh",
    "ksh",
    "desktop",
    "deb",
    "rpm",
    "appimage",
    "run",
    "bin", // scripts
    "py",
    "pyw",
    "pl",
    "rb",
];

type Key = (String, i64);

/// The open chat's files, and what is shown of them.
#[derive(Default)]
pub(super) struct Files {
    /// The open chat's messages' files, by message (author, sent at).
    by_message: HashMap<Key, Vec<Attachment>>,
    loading: bool,
    again: bool,
    /// Pictures by their file, decoded small.
    /// shortcut: every picture of the open chat's page is kept, up to 1.6 MB each (160 MB for
    /// 100 photos), until another chat opens; the Mac app keeps 32 MB, least recently shown going
    /// first. Upgrade (with GPUI's `image_cache`) if photo-heavy chats use too much.
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
    /// Opened files' decrypted copies, by file: one each, kept until their message goes (it
    /// expires, is deleted, or its chat is) or the app quits.
    copies: HashMap<i64, Copy>,
}

enum Thumb {
    Loading,
    Shown(Arc<RenderImage>),
    /// Didn't read, or isn't a picture this can decode: shown as a file to open.
    Failed,
}

struct Playing {
    id: i64,
    play: u64,
    player: rodio::Player,
    clock: Clock,
    /// The sound stops when this goes.
    _device: rodio::MixerDeviceSink,
}

/// An opened file's decrypted copy.
struct Copy {
    chat: String,
    message: Key,
    /// Its own folder, which goes with it.
    dir: PathBuf,
    /// Made (a click meanwhile waits for it).
    ready: bool,
}

/// What a playing recording shows at each tick.
#[derive(Default)]
struct Clock {
    last: Duration,
    still: u32,
    paused: u32,
    /// The second and the track's pixel shown.
    shown: Option<(u64, u32)>,
}

#[derive(Debug, PartialEq)]
enum Tick {
    /// Nothing shown changed.
    Same,
    Moved,
    Ended,
    /// It stopped moving while playing.
    Stalled,
    /// Paused for long.
    Idle,
}

impl Clock {
    fn tick(&mut self, at: Duration, length: Option<Duration>, paused: bool, ended: bool) -> Tick {
        if ended {
            return Tick::Ended;
        }
        if paused {
            self.still = 0;
            self.paused += 1;
            return if self.paused >= IDLE_TICKS {
                Tick::Idle
            } else {
                Tick::Same
            };
        }
        self.paused = 0;
        self.still = if at == self.last { self.still + 1 } else { 0 };
        self.last = at;
        if self.still >= STALL_TICKS {
            return Tick::Stalled;
        }
        let shown = Some((at.as_secs_f64().round() as u64, (TRACK * fraction(at, length)) as u32));
        if std::mem::replace(&mut self.shown, shown) == shown {
            Tick::Same
        } else {
            Tick::Moved
        }
    }
}

/// How far a recording has played, 0 to 1 (0 while its length isn't known).
fn fraction(at: Duration, length: Option<Duration>) -> f64 {
    match length {
        Some(length) if !length.is_zero() => (at.as_secs_f64() / length.as_secs_f64()).clamp(0., 1.),
        _ => 0.,
    }
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
    /// Seconds played, how far along the track (0 to 1), and how long it is (0: not known yet).
    pub elapsed: f64,
    pub fraction: f64,
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
                // one that didn't read or says it is a picture but isn't: still there to open
                Some(Thumb::Failed) => Look::File,
                Some(Thumb::Shown(thumb)) => Look::Picture(Some(thumb.clone())),
                _ => Look::Picture(None),
            };
        }
        if is_audio(a) && a.size <= PLAY_LIMIT {
            let playing = self.playing.as_ref().filter(|p| p.id == a.id);
            let at = playing.map_or(Duration::ZERO, |p| p.player.get_pos());
            let length = self.lengths.get(&a.id).copied().flatten();
            return Look::Audio(Audio {
                playing: playing.is_some_and(|p| !p.player.is_paused()),
                elapsed: at.as_secs_f64(),
                fraction: fraction(at, length),
                length: length.map_or(0., |d| d.as_secs_f64()),
                unplayable: self.unplayable.contains(&a.id),
            });
        }
        Look::File
    }

    /// Takes the copies `gone` picks out of the list: their folders, to remove.
    fn take_copies(&mut self, gone: impl Fn(&Copy) -> bool) -> Vec<PathBuf> {
        let ids: Vec<i64> = self.copies.iter().filter(|(_, c)| gone(c)).map(|(id, _)| *id).collect();
        ids.iter()
            .filter_map(|id| self.copies.remove(id))
            .map(|c| c.dir)
            .collect()
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

/// The page's files from the core's details of its messages (`keys`); a message the core
/// couldn't read isn't in them, and keeps what it showed.
fn with_details(
    shown: &HashMap<Key, Vec<Attachment>>,
    keys: &[MessageKey],
    details: Vec<MessageDetails>,
) -> HashMap<Key, Vec<Attachment>> {
    let mut read: HashMap<Key, Vec<Attachment>> = details
        .into_iter()
        .map(|d| ((d.key.author, d.key.sent_at), d.attachments))
        .collect();
    let files = keys.iter().filter_map(|k| {
        let k = (k.author.clone(), k.sent_at);
        let files = match read.remove(&k) {
            Some(files) => files,
            None => shown.get(&k)?.clone(),
        };
        (!files.is_empty()).then_some((k, files))
    });
    files.collect()
}

/// Why a recording didn't start.
enum Unplayed {
    Unreadable(String),
    Undecodable(String),
    NoOutput(String),
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
            // one call for the page
            let details = Compat::new(client.details(keys.clone())).await;
            let again = this.update(cx, |app, cx| {
                // another chat was opened meanwhile: its messages read its files
                if app.generation == generation {
                    app.files.by_message = with_details(&app.files.by_message, &keys, details);
                    app.load_media(cx);
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

    /// Decodes the pictures and reads the recordings' lengths not asked for yet, one at a time,
    /// while their chat stays open.
    fn load_media(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let (mut pictures, mut recordings) = (vec![], vec![]);
        for a in self.files.by_message.values().flatten() {
            let Some(path) = ready_path(a) else { continue };
            if is_image(a) && !self.files.thumbs.contains_key(&path) {
                self.files.thumbs.insert(path.clone(), Thumb::Loading);
                pictures.push((a.id, path));
            } else if is_audio(a) && a.size <= LENGTH_READ_LIMIT && !self.files.lengths.contains_key(&a.id) {
                self.files.lengths.insert(a.id, None);
                recordings.push((a.id, path, a.content_type.clone()));
            }
        }
        if pictures.is_empty() && recordings.is_empty() {
            return;
        }
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            for (id, path) in pictures {
                // gone with the chat it was in
                let wanted = this.update(cx, |app, _| {
                    let open = app.generation == generation;
                    (
                        open,
                        open && matches!(app.files.thumbs.get(&path), Some(Thumb::Loading)),
                    )
                });
                match wanted {
                    Ok((true, true)) => {}
                    Ok((true, false)) => continue,
                    _ => return,
                }
                // decrypted in memory only
                let thumb = match Compat::new(client.file_bytes(path.clone())).await {
                    Ok(bytes) => cx.background_spawn(async move { thumbnail(&bytes) }).await,
                    Err(e) => Err(format!("it didn't read: {e}")),
                };
                let shown = this.update(cx, |app, cx| {
                    if !matches!(app.files.thumbs.get(&path), Some(Thumb::Loading)) {
                        return;
                    }
                    let thumb = thumb.map_or_else(
                        |e| {
                            tracing::warn!(file = id, error = %e, "a picture isn't shown");
                            Thumb::Failed
                        },
                        |thumb| Thumb::Shown(Arc::new(thumb)),
                    );
                    app.files.thumbs.insert(path, thumb);
                    cx.notify();
                });
                if shown.is_err() {
                    return;
                }
            }
            for (id, path, content_type) in recordings {
                let wanted = this.update(cx, |app, _| {
                    let open = app.generation == generation;
                    (open, open && app.files.lengths.get(&id) == Some(&None))
                });
                match wanted {
                    Ok((true, true)) => {}
                    Ok((true, false)) => continue,
                    _ => return,
                }
                // unknown until played
                let found = match Compat::new(client.file_bytes(path)).await {
                    Ok(bytes) => cx.background_spawn(async move { length(bytes, &content_type) }).await,
                    Err(_) => None,
                };
                let shown = this.update(cx, |app, cx| {
                    if found.is_some() && app.files.lengths.get(&id) == Some(&None) {
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

    /// Leaving a chat: its pictures' memory goes, its recording stops, and what didn't read or
    /// play is tried again next time.
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
        self.files.lengths.clear();
        self.files.unplayable.clear();
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
    /// decrypted copy under its own name in a folder of its own, made once. A program or a
    /// script is only saved.
    pub(super) fn open_file(&mut self, a: &Attachment, cx: &mut Context<Self>) {
        if !openable(a) {
            return self.save_file(a, cx);
        }
        let (Some(client), Some(path), Some(chat)) = (self.client.clone(), ready_path(a), self.selected.clone()) else {
            return;
        };
        let name = file_name(a);
        let what = a.file_name.clone().unwrap_or_else(|| "the file".into());
        if let Some(copy) = self.files.copies.get(&a.id) {
            // still being made: it opens then
            if !copy.ready {
                return;
            }
            let file = copy.dir.join(&name);
            if file.exists() {
                return launch(file, what, cx);
            }
        }
        // none yet, or its copy went (by hand): a new one
        remove_copies(self.files.copies.remove(&a.id).map(|c| c.dir));
        let dir = self
            .data_dir
            .join(OPEN_FOLDER)
            .join(hex::encode(rand::random::<[u8; 8]>()));
        let copy = Copy {
            chat,
            message: (a.message.author.clone(), a.message.sent_at),
            dir: dir.clone(),
            ready: false,
        };
        self.files.copies.insert(a.id, copy);
        let (id, file) = (a.id, dir.join(&name));
        cx.spawn(async move |this, cx| {
            let made = match std::fs::create_dir_all(&dir) {
                Ok(()) => copy_out(&client, path, &file, true).await,
                Err(e) => Err(e.to_string()),
            };
            let _ = this.update(cx, |app, cx| {
                let copy = app.files.copies.get_mut(&id).filter(|c| c.dir == dir);
                match (made, copy) {
                    (Ok(()), Some(copy)) => {
                        copy.ready = true;
                        launch(file, what, cx);
                    }
                    // its message went meanwhile, and its copy with it
                    (Ok(()), None) => remove_copies([dir]),
                    (Err(e), copy) => {
                        if copy.is_some() {
                            app.files.copies.remove(&id);
                        }
                        remove_copies([dir]);
                        app.error = Some(format!("Couldn't open {what}: {e}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// A message changed or expired: its opened copies go once it is gone or deleted.
    pub(super) fn drop_copies_if_gone(&mut self, key: &MessageKey, cx: &mut Context<Self>) {
        let k = (key.author.clone(), key.sent_at);
        let Some(client) = self
            .client
            .clone()
            .filter(|_| self.files.copies.values().any(|c| c.message == k))
        else {
            return;
        };
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let gone = match Compat::new(client.message(key)).await {
                Ok(found) => found.is_none_or(|m| m.deleted),
                // kept: copies go at quit anyway
                Err(_) => false,
            };
            if gone {
                let _ = this.update(cx, |app, _| remove_copies(app.files.take_copies(|c| c.message == k)));
            }
        })
        .detach();
    }

    /// A chat was deleted: its opened copies go.
    pub(super) fn drop_chat_copies(&mut self, chat: &str) {
        remove_copies(self.files.take_copies(|c| c.chat == chat));
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
            let started = match Compat::new(client.file_bytes(path)).await {
                Ok(bytes) => cx.background_spawn(async move { start(bytes, &content_type) }).await,
                Err(e) => Err(Unplayed::Unreadable(e.to_string())),
            };
            let _ = this.update(cx, |app, cx| {
                // the chat was left meanwhile: the sound goes with what was started
                if app.files.opening != Some(id) {
                    return;
                }
                app.files.opening = None;
                match started {
                    Ok((device, player, length)) => {
                        if length.is_some() {
                            app.files.lengths.insert(id, length);
                        }
                        app.files.plays += 1;
                        let play = app.files.plays;
                        app.files.playing = Some(Playing {
                            id,
                            play,
                            player,
                            clock: Clock::default(),
                            _device: device,
                        });
                        app.follow(play, cx);
                    }
                    Err(Unplayed::NoOutput(e)) => app.error = Some(format!("Couldn't play the recording: {e}")),
                    Err(Unplayed::Unreadable(e) | Unplayed::Undecodable(e)) => {
                        tracing::warn!(file = id, error = %e, "a recording doesn't play");
                        app.files.unplayable.insert(id);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The play's time moves on while it plays; at its end it is ready to play again from the
    /// start.
    fn follow(&mut self, play: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            let going = this.update(cx, |app, cx| {
                let files = &mut app.files;
                let Some(p) = files.playing.as_mut().filter(|p| p.play == play) else {
                    return false;
                };
                let length = files.lengths.get(&p.id).copied().flatten();
                match p
                    .clock
                    .tick(p.player.get_pos(), length, p.player.is_paused(), p.player.empty())
                {
                    Tick::Same => return true,
                    Tick::Moved => {}
                    Tick::Ended | Tick::Idle => files.playing = None,
                    Tick::Stalled => {
                        tracing::warn!(file = p.id, "a recording stopped moving while playing");
                        files.playing = None;
                        app.error = Some("The recording stopped: the sound output went away".into());
                    }
                }
                cx.notify();
                app.files.playing.is_some()
            });
            if !going.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }
}

/// A recording decoded and playing on the system's sound output (its length, if its format says).
fn start(
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<(rodio::MixerDeviceSink, rodio::Player, Option<Duration>), Unplayed> {
    let source = decoder(bytes, content_type).map_err(|e| Unplayed::Undecodable(e.to_string()))?;
    let length = source.total_duration();
    let mut device = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| Unplayed::NoOutput(e.to_string()))?;
    device.log_on_drop(false);
    let player = rodio::Player::connect_new(device.mixer());
    player.append(source);
    Ok((device, player, length))
}

/// Opens a copy in the system's app for its kind, off the main thread; a failure is said.
fn launch(file: PathBuf, what: String, cx: &mut Context<Hush>) {
    cx.spawn(async move |this, cx| {
        let opened = cx.background_spawn(async move { open_with_system(&file) }).await;
        if let Err(e) = opened {
            let _ = this.update(cx, |app, cx| {
                app.error = Some(format!("Couldn't open {what}: {e}"));
                cx.notify();
            });
        }
    })
    .detach();
}

/// Opens `file` in the system's app for its kind. Not GPUI's: that says nothing when no app opens
/// it, and on Windows logs the file's path (a name the sender chose).
fn open_with_system(file: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut opener = {
        let mut open = Command::new("/usr/bin/open");
        open.arg("--");
        open
    };
    #[cfg(windows)]
    let mut opener = Command::new("explorer.exe");
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut opener = Command::new("xdg-open");
    // what they print names the file
    let done = opener
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    // explorer's exit code means nothing
    if done.success() || cfg!(windows) {
        Ok(())
    } else {
        Err("no app here opens this kind of file".into())
    }
}

/// Copies a previous run left open, or this one: they go, at launch (once the core holds the
/// data folder) and at quit.
pub(super) fn clear_open_copies(data_dir: &Path) {
    let root = data_dir.join(OPEN_FOLDER);
    let copies = match std::fs::read_dir(&root) {
        Ok(copies) => copies,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return tracing::warn!(error = %e, "the opened files' copies couldn't be listed"),
    };
    // each on its own: one a program still holds (Windows locks it) doesn't keep the others
    remove_copies(copies.filter_map(|c| c.ok().map(|c| c.path())));
    // left only while a copy is, which was said
    let _ = std::fs::remove_dir(&root);
}

/// Removes opened copies' folders; one that can't go is said in the log (the next launch tries
/// again).
fn remove_copies(dirs: impl IntoIterator<Item = PathBuf>) {
    for dir in dirs {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, "an opened file's copy couldn't be removed");
            }
        }
    }
}

/// Removes a half-made copy; one that can't go is said in the log.
fn remove_copy(file: &Path) {
    if let Err(e) = std::fs::remove_file(file) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "a file's unfinished copy couldn't be removed");
        }
    }
}

/// `path`'s file decrypted into a new file at `to`, marked as come from someone else. `marked`:
/// a copy that can't carry the mark goes.
async fn copy_out(client: &Client, path: String, to: &Path, marked: bool) -> Result<(), String> {
    let at = to.to_str().ok_or("the file's path isn't valid text")?.to_owned();
    Compat::new(client.copy_file(path, at))
        .await
        .map_err(|e| e.to_string())?;
    match mark_from_elsewhere(to) {
        Err(e) if marked => {
            remove_copy(to);
            Err(format!("couldn't mark its copy: {e}"))
        }
        _ => Ok(()),
    }
}

/// Written beside `to` and onto the disk first, so a file there is replaced only by a whole copy.
/// A drive that can't hold the mark (FAT) gets the copy without it, as Windows' own copies there.
async fn save(client: &Client, path: String, to: &Path) -> Result<(), String> {
    let part = to.with_file_name(format!(".{}.part", hex::encode(rand::random::<[u8; 8]>())));
    copy_out(client, path, &part, false).await?;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .and_then(|f| f.sync_all())
        .and_then(|()| std::fs::rename(&part, to));
    written.map_err(|e| {
        remove_copy(&part);
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

/// Whether a file may be handed to the system to open: not a program or a script.
pub(super) fn openable(a: &Attachment) -> bool {
    !dangerous(&file_name(a))
}

/// A program's or a script's name, by its extension (Signal Desktop's `isFileDangerous`).
fn dangerous(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| DANGEROUS.contains(&ext.to_ascii_lowercase().as_str()))
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

/// The name a file's copy gets: the sender's, only its last part, without what a name can't hold
/// on Windows or what would turn it around on screen, at most `NAME_LIMIT` bytes; or "file" with
/// its type's extension, unless that would make it a program.
fn file_name(a: &Attachment) -> String {
    let given = a.file_name.as_deref().and_then(|n| n.rsplit(['/', '\\']).next());
    let given = given.map(|n| {
        let n: String = n
            .chars()
            .map(|c| if hidden(c) || "<>:\"|?*".contains(c) { '_' } else { c })
            .collect();
        // Windows drops a name's trailing dots and spaces (and with them "." and "..")
        n.trim_end_matches(['.', ' ']).to_string()
    });
    let typed = || match extension(&a.content_type).filter(|ext| !dangerous(&format!("file.{ext}"))) {
        Some(ext) => format!("file.{ext}"),
        None => "file".into(),
    };
    let name = shortened(given.filter(|n| !n.is_empty()).unwrap_or_else(typed));
    if device(&name) {
        format!("_{name}")
    } else {
        name
    }
}

/// Control and format characters: invisible, and some turn the text after them around
/// ("invoice\u{202E}fdp.exe" reads "invoiceexe.pdf").
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{AD}'
                | '\u{61C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// A name cut to `NAME_LIMIT` bytes, its extension kept.
fn shortened(name: String) -> String {
    if name.len() <= NAME_LIMIT {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() < 16 => (stem, format!(".{ext}")),
        _ => (name.as_str(), String::new()),
    };
    let mut end = NAME_LIMIT - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", &stem[..end])
}

/// A Windows device's name, with an extension too ("con.txt", "COM¹").
fn device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            let mut n = n.chars();
            matches!((n.next(), n.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        })
    };
    ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str()) || numbered("COM") || numbered("LPT")
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
/// or why not.
fn thumbnail(bytes: &[u8]) -> Result<RenderImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_PIXELS * 4);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_DECODED_PIXELS {
        return Err(format!("it is too large to show ({w}×{h})"));
    }
    // a broken EXIF shows the picture as it is
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut picture = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if picture.width() > MAX_PIXELS || picture.height() > MAX_PIXELS {
        picture = picture.thumbnail(MAX_PIXELS, MAX_PIXELS);
    }
    // turned once small: turning the whole photo would cost as much again
    picture.apply_orientation(orientation);
    let mut pixels = picture.into_rgba8();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(RenderImage::new([image::Frame::new(pixels)]))
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
        byte_count, clear_open_copies, clock, display_size, file_name, is_audio, is_image, length, openable, thumbnail,
        with_details, Attachment, Clock, Copy, Duration, Files, Look, Thumb, Tick, PLAY_LIMIT,
    };
    use gpui_kit::{DevicePixels, Size};
    use sig_core::projection::{AttachmentState, Message, MessageDetails, MessageKey, MessageKind, MessageStatus};
    use std::collections::HashMap;
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
        assert!(thumbnail(b"HUSHFIL1 still encrypted").is_err());
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
        let podcast = Attachment {
            size: PLAY_LIMIT + 1,
            ..ready(6, "audio/mpeg")
        };
        let picture = thumbnail(&png(4, 4)).unwrap();
        files.thumbs.insert("f1".into(), Thumb::Shown(Arc::new(picture)));
        files.thumbs.insert("f2".into(), Thumb::Failed);
        files.lengths.insert(4, Some(Duration::from_secs(42)));
        files
            .by_message
            .insert(("a".into(), 1), vec![decoded, broken, coming, voice, video, podcast]);
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
        // a picture that didn't read or decode is a file to open; a video opens in the system's
        // player, and so does a recording too large to read into memory
        let expected = ["picture", "file", "placeholder", "audio 42 false", "file", "file"];
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

    #[test]
    fn names_lose_direction_marks_and_windows_devices_and_stay_short() {
        let named = |name: &str| file_name(&file("application/octet-stream", Some(name)));
        assert_eq!(named("invoice\u{202E}fdp.exe"), "invoice_fdp.exe");
        assert_eq!(named("a\u{2066}b\u{200F}c\u{FEFF}d\u{061C}.txt"), "a_b_c_d_.txt");
        assert_eq!(named("COM\u{B9}.txt"), "_COM\u{B9}.txt");
        assert_eq!(named("conin$.txt"), "_conin$.txt");
        assert_eq!(named("lpt0"), "_lpt0");
        assert_eq!(named("compact.txt"), "compact.txt");
        let long = named(&format!("{}.pdf", "é".repeat(150)));
        assert!(long.len() <= 200 && long.ends_with("é.pdf"), "{long}");
    }

    #[test]
    fn programs_and_scripts_are_never_opened_only_saved() {
        let named = |name: &str| file("application/octet-stream", Some(name));
        for name in [
            "setup.exe",
            "run.BAT",
            "x.ps1",
            "a.js",
            "b.jar",
            "c.sh",
            "d.command",
            "e.desktop",
            "f.app",
        ] {
            assert!(!openable(&named(name)), "{name}");
        }
        assert!(
            !openable(&named("invoice\u{202E}fdp.exe")),
            "hidden behind a direction mark"
        );
        assert!(!openable(&named("setup.exe. . ")), "with trailing dots");
        assert!(openable(&named("Menu.pdf")));
        // a nameless file's type doesn't make it a program
        let typed = file("application/exe", None);
        assert_eq!(file_name(&typed), "file");
        assert!(openable(&typed));
    }

    fn copy(chat: &str, sent_at: i64) -> Copy {
        Copy {
            chat: chat.into(),
            message: ("a".into(), sent_at),
            dir: format!("/open/{chat}{sent_at}").into(),
            ready: true,
        }
    }

    #[test]
    fn a_messages_or_a_chats_copies_are_taken_away_together() {
        let mut files = Files::default();
        files.copies.insert(1, copy("c:x", 1));
        files.copies.insert(2, copy("c:x", 1));
        files.copies.insert(3, copy("c:x", 2));
        files.copies.insert(4, copy("g:y", 3));
        let mut gone = files.take_copies(|c| c.message == ("a".into(), 1));
        gone.sort();
        assert_eq!(gone, ["/open/c:x1", "/open/c:x1"].map(std::path::PathBuf::from));
        assert_eq!(
            files.take_copies(|c| c.chat == "c:x"),
            ["/open/c:x2"].map(std::path::PathBuf::from)
        );
        assert_eq!(files.copies.len(), 1);
    }

    #[test]
    fn a_recording_that_stops_moving_while_playing_has_lost_its_output() {
        let at = Duration::from_millis;
        let length = Some(Duration::from_secs(14));
        let mut clock = Clock::default();
        assert_eq!(
            clock.tick(at(100), length, false, false),
            Tick::Moved,
            "the track's first pixel"
        );
        assert_eq!(
            clock.tick(at(150), length, false, false),
            Tick::Same,
            "nothing shown changed"
        );
        assert_eq!(
            clock.tick(at(200), length, false, false),
            Tick::Moved,
            "the next track pixel"
        );
        for _ in 0..1000 {
            assert_eq!(clock.tick(at(200), length, true, false), Tick::Same, "paused stays");
        }
        let mut last = Tick::Same;
        for _ in 0..30 {
            last = clock.tick(at(200), length, false, false);
        }
        assert_eq!(last, Tick::Stalled);
        assert_eq!(Clock::default().tick(at(0), length, false, true), Tick::Ended);
    }

    #[test]
    fn a_message_the_core_couldnt_read_keeps_its_files() {
        let key = |sent_at| MessageKey {
            author: "a".into(),
            sent_at,
        };
        let details = |sent_at, files: Vec<Attachment>| MessageDetails {
            key: key(sent_at),
            reactions: vec![],
            attachments: files,
            preview: None,
            poll: None,
        };
        let old = HashMap::from([
            (("a".into(), 1), vec![file("image/jpeg", None)]),
            (("a".into(), 2), vec![file("image/jpeg", None)]),
            (("a".into(), 9), vec![file("image/jpeg", None)]),
        ]);
        // 1 lost its file, 2 wasn't read, 3 got one, 9 left the page
        let found = vec![details(1, vec![]), details(3, vec![file("video/mp4", None)])];
        let new = with_details(&old, &[key(1), key(2), key(3)], found);
        let mut kept: Vec<i64> = new.keys().map(|k| k.1).collect();
        kept.sort();
        assert_eq!(kept, [2, 3]);
    }

    fn png_turned(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        use image::ImageEncoder as _;
        // a TIFF header and one IFD entry: 0x0112 (orientation), a SHORT
        let mut exif = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        exif.extend(orientation.to_be_bytes());
        exif.extend([0, 0, 0, 0, 0, 0]);
        let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 255, 255]));
        let mut out = vec![];
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(&picture, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn a_photo_is_turned_as_it_says_after_it_is_made_small() {
        assert_eq!(thumbnail(&png_turned(40, 20, 6)).unwrap().size(0), size2(20, 40));
        assert_eq!(thumbnail(&png_turned(1280, 640, 6)).unwrap().size(0), size2(320, 640));
        assert_eq!(thumbnail(&png_turned(40, 20, 1)).unwrap().size(0), size2(40, 20));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_file_no_app_opens_is_said() {
        let missing = std::env::temp_dir().join(format!("hush-desktop-none-{}", std::process::id()));
        assert!(super::open_with_system(&missing).is_err());
    }
}
