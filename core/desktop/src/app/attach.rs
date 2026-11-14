//! Files to send (the Mac app's `FileTray`, `AppStore.addFiles` and `Composer.pastedFiles`):
//! chosen with "+" or ⇧⌘A, dropped on the chat or pasted, they wait in a tray above the composer
//! and go with the next message, its text as their caption. A pasted picture is written into the
//! data folder as a PNG, and goes once the core has its copy or it leaves the tray.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Paste;
use gpui_kit::component::{h_flex, Disableable as _};
use gpui_kit::prelude::*;
use gpui_kit::*;
use image::ImageDecoder as _;
use sig_core::projection::db::MAX_ATTACHMENT;
use sig_core::projection::OutgoingFile;

use super::view::{answer_first, icon};
use super::{files, Hush, Screen};
use crate::style::Palette;

actions!(hush, [AttachFiles]);

/// Pasted pictures wait here, in the data folder (which only we can read), until they leave the
/// tray.
const PASTE_FOLDER: &str = "paste";

/// A file waiting in the tray.
pub(super) struct Outgoing {
    file: OutgoingFile,
    /// A picture's, decoded small.
    thumb: Option<Arc<RenderImage>>,
}

impl Outgoing {
    /// As the core takes it.
    pub(super) fn file(&self) -> OutgoingFile {
        self.file.clone()
    }
}

/// What ↩ sends.
#[derive(Debug, PartialEq)]
pub(super) enum Plan {
    Nothing,
    /// The edit's new words: an edit changes the text only, the files wait for the next message.
    Edit,
    /// Files are still being added: it goes once they're in, so words never go without the
    /// picture they were typed for (the Mac app adds them at once).
    Wait,
    Text,
    /// The tray's files, the words as their caption (none without words).
    Files {
        caption: Option<String>,
    },
}

/// What ↩ does with the composer's words (trimmed), an edit, files being added and the tray's.
pub(super) fn plan(body: &str, editing: bool, adding: bool, files: bool) -> Plan {
    if editing {
        if body.is_empty() {
            Plan::Nothing
        } else {
            Plan::Edit
        }
    } else if adding {
        Plan::Wait
    } else if files {
        Plan::Files {
            caption: Some(body.to_owned()).filter(|b| !b.is_empty()),
        }
    } else if body.is_empty() {
        Plan::Nothing
    } else {
        Plan::Text
    }
}

/// What a failed send says when its words went to its chat's draft instead of the composer: the
/// quote, and the files if another chat is open, are gone.
pub(super) fn unsent(e: &str, words: bool, quote: bool, files: bool) -> String {
    let quote = if quote { ", without the quote" } else { "" };
    let files_lost = if files { ", and its files weren't sent" } else { "" };
    match (words, files) {
        (true, _) => format!("{e}. Its text is back in its chat{quote}{files_lost}."),
        (false, true) => format!("{e}. Its files weren't sent."),
        (false, false) => e.to_owned(),
    }
}

/// What ⌘V attaches.
#[derive(Debug, PartialEq)]
enum Pasted<'a> {
    Files(&'a [PathBuf]),
    Picture(&'a Image),
}

/// ⇧⌘A (Ctrl elsewhere); bound before the menus are set, which show it.
pub fn keys() -> [KeyBinding; 1] {
    let key = if cfg!(target_os = "macos") {
        "cmd-shift-a"
    } else {
        "ctrl-shift-a"
    };
    [KeyBinding::new(key, AttachFiles, None)]
}

/// What `keys` does, wherever the window's focus is (as `search::shortcuts`).
pub fn shortcut(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    cx.on_action(move |_: &AttachFiles, cx| {
        let hush = hush.clone();
        // once the dispatch is over: it holds the window, which can't be updated inside it
        cx.defer(move |cx| {
            let done = window.update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.choose_files(window, cx))
            });
            if let Err(e) = done.and_then(|r| r) {
                tracing::warn!(error = %e, "the attach shortcut found no window");
            }
        });
    });
}

impl Hush {
    /// Files can join the tray: the open chat has a composer, and no edit is on (an edit changes
    /// the text only).
    fn writable(&self) -> bool {
        let open = self
            .selected
            .as_ref()
            .and_then(|id| self.chats.iter().find(|c| &c.id == id));
        open.is_some_and(|c| !answer_first(c)) && !self.actions.editing()
    }

    /// The picker can open, and the composer have the keys after it: nothing is over the chat.
    fn can_attach(&self) -> bool {
        matches!(self.screen, Screen::Chats) && self.dialog.is_none() && self.writable()
    }

    /// "+" and ⇧⌘A: the system's file picker, for several files.
    pub(super) fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_attach() {
            return;
        }
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let chosen = chosen.await;
            let _ = this.update_in(cx, |app, window, cx| {
                match chosen {
                    Ok(Ok(Some(paths))) => app.add_files(paths, generation, window, cx),
                    Ok(Err(e)) => app.error = Some(format!("Couldn't choose files: {e}")),
                    // cancelled
                    Ok(Ok(None)) | Err(_) => {}
                }
                // the picker doesn't block the window everywhere: a dialog opened meanwhile keeps
                // the keys
                if app.can_attach() {
                    app.composer.update(cx, |input, cx| input.focus(window, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Adds files to the open chat's tray, each once, described off the main thread. Ones that
    /// come too late (another chat is open, an edit started) don't.
    fn add_files(&mut self, paths: Vec<PathBuf>, generation: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.adding += 1;
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move { paths.into_iter().filter_map(describe).collect::<Vec<_>>() })
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                let mut found: Vec<Outgoing> = found
                    .into_iter()
                    .filter_map(|f| f.map_err(|e| app.error = Some(e)).ok())
                    .collect();
                if app.generation == generation && app.writable() {
                    found = admit(&mut app.tray, found);
                }
                app.discard(found, cx);
                app.added(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// An add is over: a message waiting for files goes once none is being added (not if an edit
    /// started meanwhile).
    fn added(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.adding = self.adding.saturating_sub(1);
        if self.adding == 0 && std::mem::take(&mut self.send_waiting) && !self.actions.editing() {
            self.send(window, cx);
        }
    }

    /// ⌘V in the composer: copied files or a copied picture go to the tray. False for text,
    /// which the field pastes.
    ///
    /// shortcut: GPUI reads no copied files on Linux (text and pictures only), so files copied in
    /// a file manager paste there as their paths' text; drop them or use "+". Read
    /// `text/uri-list` once GPUI's clipboard does.
    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let generation = self.generation;
        match pasted(&item) {
            None => false,
            Some(Pasted::Files(paths)) => {
                let files: Vec<PathBuf> = paths.iter().filter(|p| p.is_file()).cloned().collect();
                // folders: their names paste as text
                if files.is_empty() {
                    return false;
                }
                if self.writable() {
                    self.add_files(files, generation, window, cx);
                }
                true
            }
            Some(Pasted::Picture(_)) if !self.writable() => true,
            Some(Pasted::Picture(image)) => {
                let (dir, bytes) = (self.data_dir.join(PASTE_FOLDER), image.bytes.clone());
                self.adding += 1;
                cx.spawn_in(window, async move |this, cx| {
                    let written = cx.background_spawn(async move { write_png(&dir, &bytes) }).await;
                    let _ = this.update_in(cx, |app, window, cx| {
                        match written {
                            Ok(path) => app.add_files(vec![path], generation, window, cx),
                            Err(e) => app.error = Some(format!("Couldn't attach what you pasted: {e}")),
                        }
                        app.added(window, cx);
                        cx.notify();
                    });
                })
                .detach();
                true
            }
        }
    }

    fn remove_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let (gone, kept) = std::mem::take(&mut self.tray)
            .into_iter()
            .partition(|f| f.file.path == path);
        self.tray = kept;
        self.discard(gone, cx);
        cx.notify();
    }

    /// The tray's files, out of it: a second ↩ while they're on their way can't send them again.
    pub(super) fn take_tray(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.tray)
    }

    /// The core has `files` (`sent`), or they go back to the front of the tray, unless another
    /// chat is open.
    pub(super) fn tray_back(&mut self, id: &str, files: Vec<Outgoing>, sent: bool, cx: &mut Context<Self>) {
        if !sent && self.selected.as_deref() == Some(id) {
            let later = std::mem::replace(&mut self.tray, files);
            let again = admit(&mut self.tray, later);
            return self.discard(again, cx);
        }
        self.discard(files, cx);
    }

    /// ↩ was pressed while files are being added: the message goes once they're in.
    pub(super) fn send_when_added(&mut self) {
        self.send_waiting = true;
    }

    /// The send button shows when ↩ would do something: words, or files (not while editing).
    pub(super) fn shows_send(&self) -> bool {
        !self.composer_empty || (!self.tray.is_empty() && !self.actions.editing())
    }

    /// Leaving a chat: its tray empties, and a message waiting for files stays unsent.
    pub(super) fn clear_tray(&mut self, cx: &mut Context<Self>) {
        self.send_waiting = false;
        let files = self.take_tray();
        self.discard(files, cx);
    }

    /// Files out of the tray: pasted pictures go, and their thumbnails' memory.
    fn discard(&mut self, files: Vec<Outgoing>, cx: &mut Context<Self>) {
        forget(&self.data_dir.join(PASTE_FOLDER), &files);
        let thumbs: Vec<_> = files.into_iter().filter_map(|f| f.thumb).collect();
        if !thumbs.is_empty() {
            // once no window is being drawn, so each lets its copy go
            cx.defer(move |cx| {
                for thumb in thumbs {
                    cx.drop_image(thumb, None);
                }
            });
        }
    }

    /// "+", beside `composer` (as tall as its one-line field), for the open chat's files.
    pub(super) fn with_attach(&self, composer: Div, p: Palette, cx: &mut Context<Self>) -> Div {
        let button = Button::new("attach")
            .ghost()
            .icon(icon("plus", 15., p.text))
            .rounded(px(18.))
            .size(px(35.))
            .bg(p.field)
            .accessibility_label("Attach")
            .tooltip("Attach files")
            .disabled(self.actions.editing())
            .on_click(cx.listener(|this, _, window, cx| this.choose_files(window, cx)));
        h_flex()
            .flex_none()
            .items_end()
            .pl(px(14.))
            .child(div().flex_none().pb(px(12.)).child(button))
            .child(composer.flex_1().min_w_0().pl(px(8.)))
    }

    /// The tray above the composer (`FileTray`): each file with its thumbnail or an icon, its name
    /// and ✕.
    pub(super) fn render_tray(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        if self.tray.is_empty() {
            return None;
        }
        let chips = self.tray.iter().map(|f| {
            let path = f.file.path.clone();
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let shown = match &f.thumb {
                Some(thumb) => img(thumb.clone())
                    .size(px(36.))
                    .flex_none()
                    .rounded(px(6.))
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
                None => div()
                    .size(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("file", 20., p.text))
                    .into_any_element(),
            };
            h_flex()
                .flex_none()
                .gap(px(6.))
                .p(px(6.))
                .rounded(px(10.))
                .bg(p.field)
                .child(shown)
                .child(div().max_w(px(140.)).text_size(px(12.)).truncate().child(name.clone()))
                .child(
                    div()
                        .id(SharedString::from(format!("remove:{path}")))
                        .role(Role::Button)
                        .aria_label(format!("Remove {name}"))
                        .cursor_pointer()
                        .child(icon("circle-x", 14., p.secondary))
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_file(&path, cx))),
                )
        });
        let tray = h_flex().id("tray").gap(px(8.)).overflow_x_scroll().children(chips);
        Some(div().px(px(14.)).pt(px(8.)).child(tray))
    }

    /// The chat takes dropped files (with a frame while they're over it) and pasted ones, unless
    /// it has no composer.
    pub(super) fn with_drop(&self, chat: Div, writable: bool, p: Palette, cx: &mut Context<Self>) -> Div {
        if !writable {
            return chat;
        }
        let frame = p.bubble_out;
        chat.relative()
            .group("chat")
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, window, cx| {
                let generation = this.generation;
                this.add_files(dropped.paths().to_vec(), generation, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.paste(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .absolute()
                    .inset(px(4.))
                    .rounded(px(12.))
                    .group_drag_over::<ExternalPaths>("chat", move |s| s.border_3().border_color(frame)),
            )
    }

    /// `--snapshot`'s made-up tray: a pasted screenshot and a menu.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo_tray(&mut self) {
        let picture = files::thumbnail(&crate::demo::picture()).expect("the made-up picture decodes");
        let file = |path: &str, content_type: &str| OutgoingFile {
            path: path.into(),
            content_type: content_type.into(),
            width: None,
            height: None,
            is_voice_note: false,
        };
        self.tray = vec![
            Outgoing {
                file: file("/demo/Pasted image 3f9a1c2e.png", "image/png"),
                thumb: Some(Arc::new(picture)),
            },
            Outgoing {
                file: file("/demo/Menu for Saturday's dinner.pdf", "application/pdf"),
                thumb: None,
            },
        ];
    }
}

/// What ⌘V attaches: copied files, else a copied picture without text (a screenshot). None for
/// text, which the field pastes itself, even when it also carries a picture of itself (word
/// processors do that).
fn pasted(item: &ClipboardItem) -> Option<Pasted<'_>> {
    let entries = item.entries();
    let files = entries.iter().find_map(|e| match e {
        ClipboardEntry::ExternalPaths(paths) => Some(Pasted::Files(paths.paths())),
        _ => None,
    });
    if files.is_some() || entries.iter().any(|e| matches!(e, ClipboardEntry::String(_))) {
        return files;
    }
    entries.iter().find_map(|e| match e {
        ClipboardEntry::Image(image) => Some(Pasted::Picture(image)),
        _ => None,
    })
}

/// A file to send as the core takes it: its type by its extension, a picture's size as it shows
/// (a turned photo's swapped) and its thumbnail. None: not a file (a folder, or gone).
fn describe(path: PathBuf) -> Option<Result<Outgoing, String>> {
    let size = std::fs::metadata(&path).ok().filter(|m| m.is_file())?.len();
    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    // larger ones aren't sent: the core says so
    let bytes = (content_type.starts_with("image/") && size <= MAX_ATTACHMENT)
        .then(|| std::fs::read(&path).ok())
        .flatten();
    let shown = bytes.as_deref().and_then(pixel_size);
    let thumb = bytes.as_deref().and_then(|b| files::thumbnail(b).ok()).map(Arc::new);
    let Some(text) = path.to_str() else {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        return Some(Err(format!("Couldn't attach {name}: its path isn't valid text")));
    };
    let file = OutgoingFile {
        path: text.to_owned(),
        content_type,
        width: shown.map(|(w, _)| w),
        height: shown.map(|(_, h)| h),
        is_voice_note: false,
    };
    Some(Ok(Outgoing { file, thumb }))
}

/// A picture's pixel size as it shows: EXIF orientations 5–8 turn it a quarter.
fn pixel_size(bytes: &[u8]) -> Option<(u32, u32)> {
    use image::metadata::Orientation::{Rotate270, Rotate270FlipH, Rotate90, Rotate90FlipH};
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let turned = matches!(
        decoder.orientation(),
        Ok(Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH)
    );
    Some(if turned { (h, w) } else { (w, h) })
}

/// A pasted picture as a PNG in `dir` (made if missing), named as the Mac app names it. Any
/// format is turned into one, as on the Mac (Windows' screenshots come as BMP).
fn write_png(dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let unread = |_| "the image couldn't be read".to_string();
    let picture = image::load_from_memory(bytes).map_err(unread)?;
    let mut png = Cursor::new(vec![]);
    picture.write_to(&mut png, image::ImageFormat::Png).map_err(unread)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("Pasted image {}.png", hex::encode(rand::random::<[u8; 4]>())));
    std::fs::write(&path, png.into_inner()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Adds `found` to the tray, each file once; returns those already in it.
fn admit(tray: &mut Vec<Outgoing>, found: Vec<Outgoing>) -> Vec<Outgoing> {
    let mut again = vec![];
    for f in found {
        if tray.iter().any(|t| t.file.path == f.file.path) {
            again.push(f);
        } else {
            tray.push(f);
        }
    }
    again
}

/// Files out of the tray, sent (the core keeps its own copy) or dropped: the pictures pasted into
/// `paste` go. Files chosen or dropped from the user's folders are theirs and stay.
fn forget(paste: &Path, files: &[Outgoing]) {
    for f in files {
        // directly in it: "paste/../x" isn't
        let path = Path::new(&f.file.path);
        if path.parent() == Some(paste) {
            remove(path);
        }
    }
}

fn remove(file: &Path) {
    if let Err(e) = std::fs::remove_file(file) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "a pasted picture couldn't be removed");
        }
    }
}

/// What a previous run left pasted, or this one: it goes, at launch (once the core holds the
/// data folder) and at quit.
pub(super) fn clear_pasted(data_dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(data_dir.join(PASTE_FOLDER)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "the pasted pictures couldn't be removed");
        }
    }
}

#[cfg(test)]
mod tests {
    // not `super::*`: GPUI's `test` attribute would come with it
    use super::{admit, clear_pasted, describe, forget, pasted, plan, unsent, write_png, Outgoing, Pasted, Plan};
    use gpui_kit::{ClipboardEntry, ClipboardItem, ExternalPaths, Image, ImageFormat};
    use sig_core::projection::OutgoingFile;
    use std::path::{Path, PathBuf};

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hush-desktop-attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut out = std::io::Cursor::new(vec![]);
        picture.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// A PNG whose EXIF says to turn it a quarter (orientation 6).
    fn turned_png(width: u32, height: u32) -> Vec<u8> {
        use image::ImageEncoder as _;
        // a TIFF header and one IFD entry: 0x0112 (orientation), a SHORT
        let exif = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 255, 255]));
        let mut out = vec![];
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(&picture, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn outgoing(path: &Path) -> Outgoing {
        Outgoing {
            file: OutgoingFile {
                path: path.to_str().unwrap().into(),
                content_type: "image/png".into(),
                width: None,
                height: None,
                is_voice_note: false,
            },
            thumb: None,
        }
    }

    fn image(format: ImageFormat, bytes: Vec<u8>) -> Image {
        Image { format, bytes, id: 1 }
    }

    #[test]
    fn copied_files_paste_first_and_text_before_a_picture() {
        let files = ExternalPaths(vec![PathBuf::from("/a/Menu.pdf")].into());
        let text = ClipboardEntry::String(gpui_kit::ClipboardString::new("Menu.pdf".into()));
        let shot = image(ImageFormat::Png, png(2, 2));
        let item = |entries: Vec<ClipboardEntry>| ClipboardItem { entries };
        // a copied file comes with its name as text (macOS, Windows)
        let copied = item(vec![text.clone(), ClipboardEntry::ExternalPaths(files.clone())]);
        assert_eq!(pasted(&copied), Some(Pasted::Files(files.paths())));
        // a word processor's text comes with a picture of itself: the text is what's pasted
        let words = item(vec![ClipboardEntry::Image(shot.clone()), text.clone()]);
        assert_eq!(pasted(&words), None);
        let screenshot = item(vec![ClipboardEntry::Image(shot.clone())]);
        assert_eq!(pasted(&screenshot), Some(Pasted::Picture(&shot)));
        assert_eq!(pasted(&item(vec![text])), None);
        assert_eq!(pasted(&item(vec![])), None);
    }

    #[test]
    fn a_file_is_described_by_its_type_and_a_picture_by_its_size_as_shown() {
        let dir = folder("describe");
        let photo = dir.join("Beach.png");
        std::fs::write(&photo, png(40, 20)).unwrap();
        let turned = dir.join("Turned.PNG");
        std::fs::write(&turned, turned_png(40, 20)).unwrap();
        let menu = dir.join("Menu.pdf");
        std::fs::write(&menu, "%PDF-1.7").unwrap();
        let odd = dir.join("notes");
        std::fs::write(&odd, "?").unwrap();
        let broken = dir.join("broken.png");
        std::fs::write(&broken, "not a picture").unwrap();
        let file = |path: &Path| describe(path.to_path_buf()).unwrap().unwrap();

        let beach = file(&photo);
        assert_eq!(beach.file.content_type, "image/png");
        assert_eq!((beach.file.width, beach.file.height), (Some(40), Some(20)));
        assert!(beach.thumb.is_some());
        let turned = file(&turned);
        assert_eq!(turned.file.content_type, "image/png", "by its extension, in any case");
        assert_eq!(
            (turned.file.width, turned.file.height),
            (Some(20), Some(40)),
            "as it shows"
        );
        let menu = file(&menu);
        assert_eq!(menu.file.content_type, "application/pdf");
        assert_eq!(menu.file.path, dir.join("Menu.pdf").to_str().unwrap());
        assert!(menu.thumb.is_none() && menu.file.width.is_none() && !menu.file.is_voice_note);
        assert_eq!(file(&odd).file.content_type, "application/octet-stream");
        // still sent, as a file
        let broken = file(&broken);
        assert!(broken.thumb.is_none() && broken.file.width.is_none());
        assert!(describe(dir.clone()).is_none(), "a folder");
        assert!(describe(dir.join("gone.txt")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pasted_picture_is_written_as_a_png() {
        let dir = folder("paste").join("paste");
        let shot = write_png(&dir, &png(3, 2)).unwrap();
        let name = shot.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("Pasted image ") && name.ends_with(".png") && name.len() == 25,
            "{name}"
        );
        assert_eq!(shot.parent(), Some(dir.as_path()));
        // Windows' screenshots come as BMP
        let mut bmp = std::io::Cursor::new(vec![]);
        image::RgbImage::new(3, 2)
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .unwrap();
        let converted = write_png(&dir, bmp.get_ref()).unwrap();
        assert_ne!(converted, shot, "each its own name");
        let bytes = std::fs::read(&converted).unwrap();
        assert_eq!(image::guess_format(&bytes).unwrap(), image::ImageFormat::Png);
        assert_eq!(write_png(&dir, b"<svg/>").unwrap_err(), "the image couldn't be read");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn return_sends_files_with_their_caption_and_an_edit_only_its_words() {
        assert_eq!(plan("hi", false, false, false), Plan::Text);
        assert_eq!(plan("", false, false, false), Plan::Nothing);
        // the words are the files' caption; without words, the files go alone
        let caption = |c: Option<&str>| Plan::Files {
            caption: c.map(Into::into),
        };
        assert_eq!(plan("the menu", false, false, true), caption(Some("the menu")));
        assert_eq!(plan("", false, false, true), caption(None));
        // an edit changes the words only: the files wait for the next message
        assert_eq!(plan("fixed", true, false, true), Plan::Edit);
        assert_eq!(plan("", true, false, true), Plan::Nothing);
        assert_eq!(plan("fixed", true, true, false), Plan::Edit);
        // a file still being added: the words wait for it
        assert_eq!(plan("look", false, true, false), Plan::Wait);
        assert_eq!(plan("", false, true, true), Plan::Wait);
    }

    #[test]
    fn a_send_whose_words_went_to_a_draft_says_what_wasnt_sent() {
        let e = "The server couldn't be reached: timed out";
        assert_eq!(
            unsent(e, true, false, false),
            format!("{e}. Its text is back in its chat.")
        );
        assert_eq!(
            unsent(e, true, true, false),
            format!("{e}. Its text is back in its chat, without the quote.")
        );
        assert_eq!(
            unsent(e, true, true, true),
            format!("{e}. Its text is back in its chat, without the quote, and its files weren't sent.")
        );
        assert_eq!(unsent(e, false, false, true), format!("{e}. Its files weren't sent."));
    }

    #[test]
    fn each_file_waits_in_the_tray_once() {
        let mut tray = vec![outgoing(Path::new("/a/1.png"))];
        let again = admit(
            &mut tray,
            vec![outgoing(Path::new("/a/2.png")), outgoing(Path::new("/a/1.png"))],
        );
        let paths: Vec<&str> = tray.iter().map(|f| f.file.path.as_str()).collect();
        assert_eq!(paths, ["/a/1.png", "/a/2.png"]);
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn only_pasted_pictures_go_with_the_tray() {
        let data = folder("forget");
        let paste = data.join("paste");
        std::fs::create_dir_all(&paste).unwrap();
        let shot = paste.join("Pasted image 01234567.png");
        let mine = data.join("Holiday.png");
        // a path that only looks as if it were in the paste folder
        let climbing = paste.join("..").join("Climb.png");
        for f in [&shot, &mine, &climbing] {
            std::fs::write(f, "x").unwrap();
        }
        forget(&paste, &[outgoing(&shot), outgoing(&mine), outgoing(&climbing)]);
        let (gone, kept) = (!shot.exists(), mine.exists() && climbing.exists());
        std::fs::write(paste.join("Pasted image 89abcdef.png"), "x").unwrap();
        std::fs::write(data.join("projection.db"), "kept").unwrap();
        clear_pasted(&data);
        let cleared = !paste.exists() && data.join("projection.db").exists();
        std::fs::remove_dir_all(&data).unwrap();
        assert!(gone && kept && cleared);
        clear_pasted(&data); // nothing left: no error
    }
}

/// Attaching in a real (headless) window: keys, drops and the picker go through GPUI. Needs GPUI's
/// test support, which `snapshot` turns on: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::{Path, PathBuf};

    use gpui_kit::{
        gpui, point, px, AnyWindowHandle, AppContext as _, ClipboardEntry, ClipboardItem, ClipboardString, Entity,
        ExternalPaths, FileDropEvent, Image, ImageFormat, TestAppContext, VisualTestContext, WindowOptions,
    };

    use std::sync::Arc;

    use gpui_kit::Focusable as _;
    use sig_core::projection::db::MAX_ATTACHMENT;

    use super::super::{compose::Typed, DialogKind, Hush};
    use super::{keys, shortcut, AttachFiles, PASTE_FOLDER};
    use crate::Demo;

    struct Quiet;

    impl sig_core::EventListener for Quiet {
        fn on_event(&self, _: sig_core::CoreEvent) {}
    }

    /// A core that isn't linked: a send of files it refuses fails at once, as a linked one's would.
    fn with_core(hush: &Entity<Hush>, data: &Path, cx: &mut TestAppContext) {
        let dir = data.join("core");
        std::fs::create_dir_all(&dir).unwrap();
        let client = sig_core::Client::open(dir.to_str().unwrap().into(), "ab".repeat(32), Arc::new(Quiet)).unwrap();
        hush.update(cx, |app, _| app.client = Some(client));
    }

    /// A file larger than the core sends (sparse: no 100 MB written).
    fn too_large(dir: &Path) -> PathBuf {
        let path = dir.join("big.pdf");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_ATTACHMENT + 1)
            .unwrap();
        path
    }

    fn copied(path: &Path) -> ClipboardEntry {
        ClipboardEntry::ExternalPaths(ExternalPaths(vec![path.to_path_buf()].into()))
    }

    fn error(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.error.clone())
    }

    fn in_window(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut gpui_kit::Window, &mut gpui_kit::Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .unwrap();
    }

    /// The demo's chat, with a data folder of its own and the composer focused.
    fn open(name: &str, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, PathBuf) {
        let data = std::env::temp_dir().join(format!("hush-desktop-tray-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(keys());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens");
            shortcut(window, hush.downgrade(), cx);
            (window, hush)
        });
        let dir = data.clone();
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| {
                    app.data_dir = dir;
                    app.composer.update(cx, |c, cx| c.focus(window, cx));
                })
            })
            .unwrap();
        cx.run_until_parked();
        (window, hush, data)
    }

    fn tray(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Vec<String> {
        hush.read_with(cx, |app, _| app.tray.iter().map(|f| f.file.path.clone()).collect())
    }

    fn typed(hush: &Entity<Hush>, cx: &mut TestAppContext) -> String {
        hush.read_with(cx, |app, cx| app.composer.read(cx).value().to_string())
    }

    fn file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        path
    }

    fn text(s: &str) -> ClipboardEntry {
        ClipboardEntry::String(ClipboardString::new(s.into()))
    }

    fn screenshot() -> ClipboardEntry {
        let mut png = std::io::Cursor::new(vec![]);
        image::RgbaImage::new(4, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        ClipboardEntry::Image(Image {
            format: ImageFormat::Png,
            bytes: png.into_inner(),
            id: 7,
        })
    }

    fn paste(window: AnyWindowHandle, entries: Vec<ClipboardEntry>, cx: &mut TestAppContext) {
        cx.write_to_clipboard(ClipboardItem { entries });
        cx.simulate_keystrokes(window, "secondary-v");
        cx.run_until_parked();
    }

    /// The pictures pasted so far.
    fn pasted(data: &Path) -> usize {
        std::fs::read_dir(data.join(PASTE_FOLDER)).map_or(0, |d| d.count())
    }

    #[gpui::test]
    fn pasting_files_or_a_picture_fills_the_tray_and_text_still_pastes(cx: &mut TestAppContext) {
        let (window, hush, data) = open("paste", cx);
        let menu = file(&data, "Menu.pdf");
        // a copied file comes with its name as text: the file is what's pasted
        let copied = ExternalPaths(vec![menu.clone()].into());
        paste(
            window,
            vec![ClipboardEntry::ExternalPaths(copied), text("Menu.pdf")],
            cx,
        );
        assert_eq!(tray(&hush, cx), [menu.to_str().unwrap()]);
        assert_eq!(typed(&hush, cx), "");

        // words with a picture of themselves (a word processor's) paste as words
        paste(window, vec![screenshot(), text("see you")], cx);
        assert_eq!(typed(&hush, cx), "see you");
        assert_eq!(tray(&hush, cx).len(), 1);

        // a screenshot: a PNG of ours, in the data folder
        paste(window, vec![screenshot()], cx);
        let files = tray(&hush, cx);
        assert_eq!(files.len(), 2);
        let shot = PathBuf::from(&files[1]);
        assert_eq!(shot.parent(), Some(data.join(PASTE_FOLDER).as_path()));
        assert!(shot.exists());

        // out of the tray: ours goes, the user's stays
        for f in &files {
            hush.update(cx, |app, cx| app.remove_file(f, cx));
        }
        cx.run_until_parked();
        assert!(tray(&hush, cx).is_empty());
        assert!(!shot.exists() && menu.exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn files_dropped_on_the_chat_wait_in_the_tray_each_once(cx: &mut TestAppContext) {
        let (window, hush, data) = open("drop", cx);
        let (a, b) = (file(&data, "a.txt"), file(&data, "b.txt"));
        // a folder isn't sent
        let paths = ExternalPaths(vec![a.clone(), data.clone(), b.clone(), a.clone()].into());
        let mut cx = VisualTestContext::from_window(window, cx);
        let position = point(px(700.), px(300.));
        cx.simulate_event(FileDropEvent::Entered { position, paths });
        cx.simulate_event(FileDropEvent::Submit { position });
        cx.run_until_parked();
        assert_eq!(tray(&hush, &mut cx), [a.to_str().unwrap(), b.to_str().unwrap()]);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_attach_shortcut_opens_the_picker_unless_an_edit_is_on(cx: &mut TestAppContext) {
        let (window, hush, data) = open("picker", cx);
        let menu = file(&data, "Menu.pdf");
        cx.simulate_keystrokes(window, "secondary-shift-a");
        assert!(cx.did_prompt_for_paths());
        let chosen = menu.clone();
        cx.simulate_path_prompt_response(move |options| {
            assert!(options.files && options.multiple && !options.directories);
            Some(vec![chosen])
        });
        cx.run_until_parked();
        assert_eq!(tray(&hush, cx), [menu.to_str().unwrap()]);

        // an edit changes the text only: no picker, and nothing pasted joins the tray
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| {
                    let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
                    app.start_editing(&ours.expect("one of ours"), window, cx);
                })
            })
            .unwrap();
        cx.dispatch_action(window, AttachFiles);
        assert!(!cx.did_prompt_for_paths());
        paste(window, vec![screenshot()], cx);
        assert_eq!(tray(&hush, cx).len(), 1);
        assert_eq!(pasted(&data), 0);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_pickers_answer_leaves_a_dialog_opened_meanwhile_its_keys(cx: &mut TestAppContext) {
        let (window, hush, data) = open("dialog", cx);
        let menu = file(&data, "Menu.pdf");
        cx.simulate_keystrokes(window, "secondary-shift-a");
        assert!(cx.did_prompt_for_paths());
        // the picker doesn't block the window everywhere (macOS)
        in_window(window, &hush, cx, |app, window, cx| {
            app.open_dialog(DialogKind::Username, window, cx)
        });
        let chosen = menu.clone();
        cx.simulate_path_prompt_response(move |_| Some(vec![chosen]));
        cx.run_until_parked();
        let field = hush.read_with(cx, |app, cx| app.dialog.as_ref().unwrap().field.focus_handle(cx));
        let kept = window.update(cx, |_, window, _| field.is_focused(window)).unwrap();
        assert!(kept, "the dialog keeps the keys");
        assert_eq!(
            tray(&hush, cx),
            [menu.to_str().unwrap()],
            "the files wait in the chat's tray"
        );
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn the_send_button_shows_for_files_but_not_for_an_edit_without_words(cx: &mut TestAppContext) {
        let (window, hush, data) = open("button", cx);
        let shows = |cx: &mut TestAppContext| hush.read_with(cx, |app, _| app.shows_send());
        assert!(!shows(cx));
        paste(window, vec![copied(&file(&data, "Menu.pdf"))], cx);
        assert!(shows(cx), "files without words");
        in_window(window, &hush, cx, |app, window, cx| {
            let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
            app.start_editing(&ours.expect("one of ours"), window, cx);
        });
        assert!(shows(cx), "Save edit");
        in_window(window, &hush, cx, |app, window, cx| {
            app.set_typed(Typed::default(), window, cx)
        });
        assert!(!shows(cx), "an edit without words: ↩ does nothing, the files wait");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn a_failed_send_brings_back_its_words_files_and_reply_in_the_mac_apps_words(cx: &mut TestAppContext) {
        let (window, hush, data) = open("failed", cx);
        let big = too_large(&data);
        paste(window, vec![copied(&big)], cx);
        with_core(&hush, &data, cx);
        in_window(window, &hush, cx, |app, window, cx| {
            let theirs = app
                .messages
                .iter()
                .rev()
                .find(|m| !m.is_outgoing && !m.deleted)
                .cloned();
            app.reply(&theirs.expect("one of theirs"), window, cx);
            app.set_typed(Typed::plain("the menu".into()), window, cx);
        });
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        cx.run_until_parked();
        // the files went, the words as their caption: the core refused the file
        assert_eq!(error(&hush, cx).as_deref(), Some("big.pdf is larger than 100 MB"));
        assert_eq!(tray(&hush, cx), [big.to_str().unwrap()]);
        assert_eq!(typed(&hush, cx), "the menu");
        let (empty, bar) = hush.read_with(cx, |app, _| (app.composer_empty, app.composer_bar().map(|b| b.0)));
        assert!(!empty);
        assert_eq!(bar, Some("reply"), "the reply is back too");

        // files alone: they come back with no words, and the send button stays for them
        in_window(window, &hush, cx, |app, window, cx| {
            app.set_typed(Typed::default(), window, cx)
        });
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        cx.run_until_parked();
        assert_eq!(tray(&hush, cx), [big.to_str().unwrap()]);
        let (empty, shows) = hush.read_with(cx, |app, _| (app.composer_empty, app.shows_send()));
        assert!(empty && shows);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn return_while_a_file_is_being_added_sends_the_words_with_it(cx: &mut TestAppContext) {
        let (window, hush, data) = open("wait", cx);
        let big = too_large(&data);
        with_core(&hush, &data, cx);
        cx.write_to_clipboard(ClipboardItem {
            entries: vec![copied(&big)],
        });
        // ⌘V then ↩ at once: the file is still being read
        in_window(window, &hush, cx, |app, window, cx| {
            app.set_typed(Typed::plain("the menu".into()), window, cx);
            assert!(app.paste(window, cx));
            app.send(window, cx);
        });
        assert_eq!(typed(&hush, cx), "the menu", "the words wait for the file");
        cx.run_until_parked();
        // then went with it (the core refused the file, so both are back)
        assert_eq!(error(&hush, cx).as_deref(), Some("big.pdf is larger than 100 MB"));
        assert_eq!(tray(&hush, cx), [big.to_str().unwrap()]);
        assert_eq!(typed(&hush, cx), "the menu");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[gpui::test]
    fn leaving_the_chat_empties_its_tray_and_a_failed_send_refills_it_first(cx: &mut TestAppContext) {
        let (window, hush, data) = open("leave", cx);
        let menu = file(&data, "Menu.pdf");
        paste(window, vec![screenshot()], cx);
        paste(
            window,
            vec![ClipboardEntry::ExternalPaths(ExternalPaths(vec![menu.clone()].into()))],
            cx,
        );
        assert_eq!(tray(&hush, cx).len(), 2);

        // taken to be sent; something else is added meanwhile; the send fails
        let taken = hush.update(cx, |app, _| app.take_tray());
        let later = file(&data, "later.txt");
        paste(
            window,
            vec![ClipboardEntry::ExternalPaths(ExternalPaths(vec![later.clone()].into()))],
            cx,
        );
        hush.update(cx, |app, cx| app.tray_back("g:01", taken, false, cx));
        let back = tray(&hush, cx);
        assert_eq!(back.len(), 3);
        assert_eq!(back[1..], [menu.to_str().unwrap(), later.to_str().unwrap()]);
        assert_eq!(pasted(&data), 1, "kept while it waits");

        // another chat: the tray empties, and the pasted picture goes
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.select("g:03".into(), window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        assert!(tray(&hush, cx).is_empty());
        assert_eq!(pasted(&data), 0);
        assert!(menu.exists() && later.exists());

        // a send that went: the core has its copy, ours goes
        paste(window, vec![screenshot()], cx);
        let sent = hush.update(cx, |app, _| app.take_tray());
        hush.update(cx, |app, cx| app.tray_back("g:03", sent, true, cx));
        assert!(tray(&hush, cx).is_empty());
        assert_eq!(pasted(&data), 0);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
