//! View-once photos and videos: one look, then gone (the Mac app's `ViewOnce` in
//! `Placeholder.swift`, and its bubble in `ConversationView.swift`; spec
//! `2026-10-17-m4-view-once-design.md`). The bubble shows a button in place of the files; a click
//! makes the look ready, shows it, and only then spends it (`Client::view_once_opened`), which
//! drops the files and tells our phone. A failed open keeps it for another try.
//!
//! Nothing of it is kept: a photo is decoded in memory and shown over the window, never in the
//! files' picture cache or on disk, and its pixels go when it closes. A video plays in the
//! system's player (GPUI plays none) from a decrypted copy in its own folder under the data
//! folder's opened copies, removed when its window here closes, else at quit or the next launch.
//! Neither offers copy, save or forward.

use std::path::PathBuf;
use std::sync::Arc;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{Attachment, AttachmentState, Message, MessageKey, MessageKind};

use super::{files, Hush};

mod view;

pub(super) use view::bubble;

/// A photo is shown at most this many pixels either way: a large screen's, at 2×.
const VIEWER_PIXELS: u32 = 2560;

type Key = (String, i64);

/// The look being made ready, or shown.
#[derive(Default)]
pub(super) struct ViewOnce {
    /// The message whose look is being made ready: its button waits.
    opening: Option<Key>,
    viewer: Option<Viewer>,
}

/// A look over the window, until closed.
pub(super) struct Viewer {
    key: Key,
    look: Look,
    /// Takes the keys from the composer: Esc closes it.
    focus: FocusHandle,
}

enum Look {
    Photo(Arc<RenderImage>),
    /// The folder of its copy, which goes with it.
    Video(PathBuf),
}

/// What a view-once message's bubble offers.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum State {
    /// Ours: we can't open it.
    Sent,
    Viewed,
    Download(Attachment),
    Downloading,
    /// Its file can't be fetched (too big for the core, say).
    Unavailable,
    /// The one look, to take.
    Open(Attachment),
    /// Being made ready, or showing: its button waits.
    Viewing(Attachment),
}

/// `m`'s state with its files as the core has them; `viewing`: its look is being made ready or
/// shows. What the core says wins: viewed here, on the phone or before a restart, it stays viewed.
pub(super) fn state(m: &Message, files: &[Attachment], viewing: bool) -> State {
    if m.is_outgoing {
        return State::Sent;
    }
    let (MessageKind::ViewOnce { viewed: false }, Some(file)) = (&m.kind, files.first()) else {
        return State::Viewed;
    };
    let file = file.clone();
    match file.state {
        _ if viewing => State::Viewing(file),
        AttachmentState::Ready => State::Open(file),
        AttachmentState::Downloading => State::Downloading,
        AttachmentState::Pending | AttachmentState::Failed if file.available => State::Download(file),
        AttachmentState::Pending | AttachmentState::Failed => State::Unavailable,
    }
}

/// The bubble's words (`ViewOnce.label`).
pub(super) fn label(state: &State) -> &'static str {
    match state {
        State::Sent => "View-once media",
        State::Viewed => "Viewed",
        State::Download(_) => "Download view-once media",
        State::Downloading => "Downloading…",
        State::Unavailable => "View-once media can't be downloaded",
        State::Open(file) | State::Viewing(file) if is_video(file) => "View video",
        State::Open(_) | State::Viewing(_) => "View photo",
    }
}

/// The name of a view-once video's copy for the system's player: from its type, never the
/// sender's name (which could make it a program or a script); none for any other type, which
/// isn't opened.
fn video_name(a: &Attachment) -> Option<String> {
    let ext = match a.content_type.split(';').next().unwrap_or_default().trim() {
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/x-m4v" => "m4v",
        "video/webm" => "webm",
        "video/3gpp" => "3gp",
        _ => return None,
    };
    Some(format!("video.{ext}"))
}

fn is_video(a: &Attachment) -> bool {
    a.content_type.starts_with("video/")
}

impl Hush {
    /// A view-once message's state, in place of its files and words; none for any other message
    /// or a deleted one.
    pub(super) fn view_once_state(&self, m: &Message) -> Option<State> {
        if m.deleted || !matches!(m.kind, MessageKind::ViewOnce { .. }) {
            return None;
        }
        let k = (m.author.clone(), m.sent_at);
        let v = &self.view_once;
        let viewing = v.opening.as_ref() == Some(&k) || v.viewer.as_ref().is_some_and(|w| w.key == k);
        Some(state(m, self.files.attachments(m), viewing))
    }

    /// "View photo" or "View video": its look is made ready (a photo decoded in memory, a video's
    /// copy made and handed to the system's player), shown, then spent. One at a time.
    pub(super) fn view_once(&mut self, a: &Attachment, window: &mut Window, cx: &mut Context<Self>) {
        let v = &self.view_once;
        if v.opening.is_some() || v.viewer.is_some() {
            return;
        }
        let (Some(client), Some(path)) = (self.client.clone(), files::ready_path(a)) else {
            return;
        };
        let video = is_video(a);
        let name = video_name(a);
        if video && name.is_none() {
            self.error = Some("Couldn't open the video: it isn't a kind of video Hush plays".into());
            return cx.notify();
        }
        let key = a.message.clone();
        self.view_once.opening = Some((key.author.clone(), key.sent_at));
        let generation = self.generation;
        let dir = self
            .data_dir
            .join(files::OPEN_FOLDER)
            .join(hex::encode(rand::random::<[u8; 8]>()));
        let file = dir.join(name.unwrap_or_default());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let look = if video {
                video_copy(&client, path, &dir, &file).await.map(|()| Look::Video(dir))
            } else {
                match Compat::new(client.file_bytes(path)).await {
                    Ok(bytes) => cx
                        .background_spawn(async move { files::picture(&bytes, VIEWER_PIXELS) })
                        .await
                        .map(|p| Look::Photo(Arc::new(p))),
                    Err(e) => Err(e.to_string()),
                }
            };
            // the player opens only if its chat is still open
            let open = this.update(cx, |app, _| app.generation == generation).unwrap_or(false);
            let look = match look {
                Ok(Look::Video(dir)) if open => {
                    match cx.background_spawn(async move { files::open_with_system(&file) }).await {
                        Ok(()) => Ok(Look::Video(dir)),
                        Err(e) => {
                            files::remove_copies([dir]);
                            Err(e)
                        }
                    }
                }
                look => look,
            };
            // the window went: a copy left goes at quit or the next launch
            let _ = this.update_in(cx, |app, window, cx| {
                app.looked(key, look, generation, video, window, cx)
            });
        })
        .detach();
    }

    /// The look is ready (or isn't): it shows and is spent. A chat left meanwhile shows nothing
    /// and spends nothing (`ConversationView.visible`).
    fn looked(
        &mut self,
        key: MessageKey,
        look: Result<Look, String>,
        generation: u64,
        video: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view_once.opening = None;
        cx.notify();
        let look = match look {
            Ok(look) if self.generation != generation => return let_go(look, cx),
            Ok(look) => look,
            Err(e) => {
                let what = if video { "the video" } else { "the photo" };
                self.error = Some(format!("Couldn't open {what}: {e}"));
                return;
            }
        };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.view_once.viewer = Some(Viewer {
            key: (key.author.clone(), key.sent_at),
            look,
            focus,
        });
        self.change("mark the view-once media opened", cx, move |client| async move {
            client.view_once_opened(key).await
        });
    }

    /// Close (or Esc): the look goes, the photo's pixels with it, the video's copy from the disk.
    pub(super) fn close_view_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(viewer) = self.view_once.viewer.take() else {
            return;
        };
        let_go(viewer.look, cx);
        self.composer.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }
}

/// A video's decrypted copy at `file`, in its new folder `dir` (removed if it fails).
async fn video_copy(
    client: &sig_core::Client,
    path: String,
    dir: &std::path::Path,
    file: &std::path::Path,
) -> Result<(), String> {
    let made = match std::fs::create_dir_all(dir) {
        Ok(()) => files::copy_out(client, path, file, true).await,
        Err(e) => Err(e.to_string()),
    };
    made.inspect_err(|_| files::remove_copies([dir.to_path_buf()]))
}

/// A look's pixels go (once no window is being drawn, as GPUI asks), or its copy.
fn let_go(look: Look, cx: &mut App) {
    match look {
        Look::Photo(photo) => cx.defer(move |cx| cx.drop_image(photo, None)),
        Look::Video(dir) => files::remove_copies([dir]),
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s photo, shown over the polls' chat.
    pub(super) fn demo_view_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let photo = files::picture(&crate::demo::picture(), VIEWER_PIXELS).expect("the made-up picture decodes");
        let last = self.messages.last().map(|m| (m.author.clone(), m.sent_at));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.view_once.viewer = Some(Viewer {
            key: last.unwrap_or_default(),
            look: Look::Photo(Arc::new(photo)),
            focus,
        });
    }
}

#[cfg(test)]
mod tests {
    use sig_core::projection::{Attachment, AttachmentState, Message, MessageKey, MessageKind, MessageStatus};

    use super::{label, state, video_name, State};
    use crate::app::actions::{menu, Item};

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

    fn file(state: AttachmentState, content_type: &str) -> Attachment {
        Attachment {
            id: 7,
            message: MessageKey {
                author: "a".into(),
                sent_at: 1,
            },
            content_type: content_type.into(),
            file_name: None,
            size: 1,
            width: None,
            height: None,
            is_voice_note: false,
            path: (state == AttachmentState::Ready).then(|| "/x".into()),
            state,
            available: true,
        }
    }

    #[test]
    fn view_once_media_offers_one_look_then_says_viewed() {
        let unopened = message(MessageKind::ViewOnce { viewed: false });
        let photo = file(AttachmentState::Ready, "image/jpeg");
        // unopened
        let open = state(&unopened, std::slice::from_ref(&photo), false);
        assert_eq!(open, State::Open(photo.clone()));
        assert_eq!(label(&open), "View photo");
        // viewing: its button waits, saying the same
        let viewing = state(&unopened, std::slice::from_ref(&photo), true);
        assert_eq!(viewing, State::Viewing(photo.clone()));
        assert_eq!(label(&viewing), "View photo");
        // opened: the core marked it viewed and dropped its file, even while it still shows
        let opened = message(MessageKind::ViewOnce { viewed: true });
        assert_eq!(state(&opened, &[], true), State::Viewed);
        assert_eq!(label(&State::Viewed), "Viewed");
        // after a restart the core says the same: viewed, whatever was listed before
        assert_eq!(state(&opened, std::slice::from_ref(&photo), false), State::Viewed);
        assert_eq!(state(&unopened, &[], false), State::Viewed, "nothing left to open");

        let video = file(AttachmentState::Ready, "video/mp4");
        assert_eq!(label(&state(&unopened, &[video], false)), "View video");
    }

    #[test]
    fn a_file_not_here_yet_is_downloaded_first() {
        let unopened = message(MessageKind::ViewOnce { viewed: false });
        for s in [AttachmentState::Pending, AttachmentState::Failed] {
            let f = file(s, "image/jpeg");
            assert_eq!(
                state(&unopened, std::slice::from_ref(&f), false),
                State::Download(f.clone())
            );
        }
        let coming = state(&unopened, &[file(AttachmentState::Downloading, "image/jpeg")], false);
        assert_eq!(coming, State::Downloading);
        let mut gone = file(AttachmentState::Pending, "video/mp4");
        gone.available = false;
        let gone = state(&unopened, &[gone], false);
        assert_eq!(gone, State::Unavailable);
        assert_eq!(label(&gone), "View-once media can't be downloaded");
        assert_eq!(
            label(&State::Download(file(AttachmentState::Pending, "image/jpeg"))),
            "Download view-once media"
        );
    }

    #[test]
    fn a_videos_copy_is_named_by_its_type_never_by_its_sender() {
        let named = |content_type: &str, name: &str| Attachment {
            file_name: Some(name.into()),
            ..file(AttachmentState::Ready, content_type)
        };
        // a sender's name could make the player's click run a program or a script
        assert_eq!(
            video_name(&named("video/mp4", "clip.command")).as_deref(),
            Some("video.mp4")
        );
        assert_eq!(video_name(&named("video/mp4", "x.app")).as_deref(), Some("video.mp4"));
        assert_eq!(
            video_name(&named("video/quicktime", "a.exe")).as_deref(),
            Some("video.mov")
        );
        assert_eq!(
            video_name(&named("video/webm; codecs=vp9", "a.sh")).as_deref(),
            Some("video.webm")
        );
        // a type that isn't a video this hands to a player: not opened at all
        assert_eq!(video_name(&named("video/x-msdownload", "a.mp4")), None);
        assert_eq!(video_name(&named("application/x-sh", "a.mp4")), None);
    }

    #[test]
    fn ours_cant_be_opened_and_none_can_be_copied() {
        let mut ours = message(MessageKind::ViewOnce { viewed: true });
        ours.is_outgoing = true;
        assert_eq!(state(&ours, &[], false), State::Sent);
        assert_eq!(label(&State::Sent), "View-once media");
        // the core keeps no caption: there is nothing to copy
        let theirs = message(MessageKind::ViewOnce { viewed: false });
        assert!(!menu(&theirs, Some("me"), 1).contains(&Item::Copy));
    }
}

/// The look in a real (headless) window, over a core that isn't linked:
/// `cargo test --features hush-desktop/snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::projection::{Attachment, MessageDetails, MessageKind};

    use super::super::attach::window_tests::with_core;
    use super::super::{files, Hush};
    use super::{Look, State};
    use crate::Demo;

    fn act(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut Window, &mut Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .expect("the window is open");
        cx.run_until_parked();
    }

    /// Until `done`, as the core answers from its own threads.
    fn until(hush: &Entity<Hush>, cx: &mut TestAppContext, done: impl Fn(&Hush) -> bool) {
        for _ in 0..200 {
            cx.run_until_parked();
            if hush.read_with(cx, |app, _| done(app)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("it didn't happen");
    }

    /// The polls' chat over a core, with Ana's view-once photo a real file (a plain one: the core
    /// reads a file outside its folder as it is) at `photo`.
    fn open(name: &str, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>, PathBuf, Attachment) {
        let data = std::env::temp_dir().join(format!("hush-desktop-once-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let photo = data.join("photo.png");
        std::fs::write(&photo, crate::demo::picture()).unwrap();
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Polls, window, cx))
            })
            .expect("the window opens")
        });
        with_core(&hush, &data, cx);
        cx.executor().allow_parking();
        let file = hush.update(cx, |app, cx| {
            app.data_dir = data.clone();
            let m = app.messages.last().cloned().unwrap();
            assert_eq!(m.kind, MessageKind::ViewOnce { viewed: false });
            let file = Attachment {
                path: Some(photo.to_str().unwrap().into()),
                ..app.files.attachments(&m)[0].clone()
            };
            let details = MessageDetails {
                key: m.key(),
                reactions: vec![],
                attachments: vec![file.clone()],
                preview: None,
                poll: None,
            };
            app.files_loaded(&[details], cx);
            file
        });
        (window, hush, data, file)
    }

    fn state(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<State> {
        hush.read_with(cx, |app, _| app.view_once_state(app.messages.last().unwrap()))
    }

    fn copies(data: &Path) -> usize {
        std::fs::read_dir(data.join(files::OPEN_FOLDER)).map_or(0, |d| d.count())
    }

    #[gpui::test]
    fn a_photo_shows_once_from_memory_and_nothing_of_it_stays(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("photo", cx);
        assert_eq!(state(&hush, cx), Some(State::Open(file.clone())));
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        until(&hush, cx, |app| app.view_once.viewer.is_some());
        assert_eq!(state(&hush, cx), Some(State::Viewing(file.clone())), "its button waits");
        // a second click while it shows takes no second look
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        assert!(
            hush.read_with(cx, |app, _| app.view_once.opening.is_none()),
            "nothing more is read"
        );
        // spent: this core isn't linked, so it says it couldn't (a linked one drops the file)
        until(&hush, cx, |app| app.error.is_some());
        let error = hush.read_with(cx, |app, _| app.error.clone()).unwrap();
        assert!(
            error.starts_with("Couldn't mark the view-once media opened: "),
            "{error}"
        );
        let photo = hush.read_with(cx, |app, _| match &app.view_once.viewer.as_ref().unwrap().look {
            Look::Photo(p) => Arc::downgrade(p),
            Look::Video(_) => panic!("a photo"),
        });
        // never in the pictures decoded for the bubbles, never on disk
        let path = file.path.clone().unwrap();
        assert!(!hush.read_with(cx, |app, _| app.files.has_picture(&path)));
        assert_eq!(copies(&data), 0);

        act(window, &hush, cx, |app, window, cx| app.close_view_once(window, cx));
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
        assert!(photo.upgrade().is_none(), "its pixels are let go");
        assert!(!hush.read_with(cx, |app, _| app.files.has_picture(&path)));
        assert_eq!(copies(&data), 0);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_look_that_doesnt_open_isnt_spent(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("broken", cx);
        // not a picture
        std::fs::write(file.path.as_deref().unwrap(), b"not a photo").unwrap();
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        until(&hush, cx, |app| app.error.is_some());
        let error = hush.update(cx, |app, _| app.error.take()).unwrap();
        assert!(error.starts_with("Couldn't open the photo: "), "{error}");
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
        // another try
        assert_eq!(state(&hush, cx), Some(State::Open(file.clone())));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_video_of_a_kind_no_player_should_open_is_never_copied_or_opened(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("refused", cx);
        let file = Attachment {
            content_type: "video/x-msdownload".into(),
            file_name: Some("run.command".into()),
            ..file
        };
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        let error = hush.update(cx, |app, _| app.error.take()).unwrap_or_default();
        assert_eq!(error, "Couldn't open the video: it isn't a kind of video Hush plays");
        assert_eq!(copies(&data), 0, "nothing copied out");
        assert!(hush.read_with(cx, |app, _| app.view_once.opening.is_none()
            && app.view_once.viewer.is_none()));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_videos_copy_goes_when_it_closes(cx: &mut TestAppContext) {
        let (window, hush, data, _) = open("video", cx);
        // as the copy `view_once` makes for the player (which a test doesn't start)
        let dir = data.join(files::OPEN_FOLDER).join("0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.mp4"), b"video").unwrap();
        act(window, &hush, cx, |app, _, cx| {
            app.view_once.viewer = Some(super::Viewer {
                key: ("a".into(), 1),
                look: Look::Video(dir.clone()),
                focus: cx.focus_handle(),
            })
        });
        act(window, &hush, cx, |app, window, cx| app.close_view_once(window, cx));
        assert!(!dir.exists(), "its copy is gone");
        let _ = std::fs::remove_dir_all(&data);
    }
}
