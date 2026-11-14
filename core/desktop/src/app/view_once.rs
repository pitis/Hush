//! View-once photos and videos: one look, then gone (the Mac app's `ViewOnce` in
//! `Placeholder.swift`, and its bubble in `ConversationView.swift`; spec
//! `2026-10-17-m4-view-once-design.md`). The bubble shows a button in place of the files; a click
//! makes the look ready, shows it, and only then spends it (`Client::view_once_opened`), which
//! drops the files and tells our phone. A failed open keeps it for another try.
//!
//! Nothing of it is kept: a photo is decoded in memory and shown over the window, never in the
//! files' picture cache or on disk, and its pixels go when it closes. A video plays in the
//! system's player (GPUI plays none) from a decrypted copy named by its type, in its own folder
//! under the data folder's opened copies, removed when its window here closes (or its chat does);
//! one the player still holds (Windows locks it) is said, and goes at quit or the next launch.
//! Hush offers no copy, save or forward for either; a video handed to the system's player is out
//! of Hush's hands. Once shown, a look's button waits for the rest of the run, even if the core
//! couldn't mark it viewed (the Mac app's `viewOnceOpening`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{Attachment, AttachmentState, Message, MessageKey, MessageKind};

use super::{files, Hush};

mod view;

pub(super) use view::bubble;

/// A photo is decoded at most this many pixels either way, then fitted to the window: a 1440p
/// screen's width, or 1280 points at 2×.
const VIEWER_PIXELS: u32 = 2560;

type Key = (String, i64);

/// The look being made ready, or shown.
#[derive(Default)]
pub(super) struct ViewOnce {
    /// The message whose look is being made ready.
    opening: Option<Key>,
    viewer: Option<Viewer>,
    /// The messages whose look was shown in this run.
    spent: HashSet<Key>,
}

impl ViewOnce {
    /// A look is over the window: the shortcuts wait for it.
    pub(super) fn showing(&self) -> bool {
        self.viewer.is_some()
    }
}

/// A look over the window, until closed.
pub(super) struct Viewer {
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
    /// Its look is being made ready, shows or was shown, or another's is: its button waits.
    Viewing(Attachment),
}

/// `m`'s state with its files as the core has them; `viewing`: a look is being made ready or shows,
/// or `m`'s was shown. What the core says wins: viewed here, on the phone or before a restart, it
/// stays viewed.
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

/// Where a view-once video's copy for the system's player goes: a new folder among the opened
/// copies, named by its type, never by the sender's name (which could make it a program or a
/// script); none for any other type, which isn't opened.
fn video_copy_path(data_dir: &Path, a: &Attachment) -> Option<PathBuf> {
    let ext = match a.content_type.split(';').next().unwrap_or_default().trim() {
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/x-m4v" => "m4v",
        "video/webm" => "webm",
        "video/3gpp" => "3gp",
        _ => return None,
    };
    let dir = data_dir
        .join(files::OPEN_FOLDER)
        .join(hex::encode(rand::random::<[u8; 8]>()));
    Some(dir.join(format!("video.{ext}")))
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
        let v = &self.view_once;
        // one look at a time: a button that can't open now waits
        let viewing = v.opening.is_some() || v.viewer.is_some() || v.spent.contains(&(m.author.clone(), m.sent_at));
        Some(state(m, self.files.attachments(m), viewing))
    }

    /// "View photo" or "View video": its look is made ready (a photo decoded in memory, a video's
    /// copy made and handed to the system's player), shown, then spent. One at a time.
    pub(super) fn view_once(&mut self, a: &Attachment, window: &mut Window, cx: &mut Context<Self>) {
        let key = a.message.clone();
        let k = (key.author.clone(), key.sent_at);
        let v = &self.view_once;
        if v.opening.is_some() || v.viewer.is_some() || v.spent.contains(&k) {
            return;
        }
        let (Some(client), Some(path)) = (self.client.clone(), files::ready_path(a)) else {
            return;
        };
        let video = is_video(a);
        let copy = video_copy_path(&self.data_dir, a);
        if video && copy.is_none() {
            self.error = Some("Couldn't open the video: it isn't a kind of video Hush plays".into());
            return cx.notify();
        }
        self.view_once.opening = Some(k);
        let generation = self.generation;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let look = match &copy {
                Some(file) => video_copy(&client, path, file).await.map(|dir| Some(Look::Video(dir))),
                None => match Compat::new(client.file_bytes(path)).await {
                    Ok(bytes) => cx
                        .background_spawn(async move { files::picture(&bytes, VIEWER_PIXELS) })
                        .await
                        .map(|p| Some(Look::Photo(Arc::new(p)))),
                    Err(e) => Err(super::describe(&e)),
                },
            };
            // the player opens only for a chat still open; once it has the file, the look counts
            // as shown wherever the window is
            let open = this.update(cx, |app, _| app.generation == generation).unwrap_or(false);
            let look = match look {
                Ok(Some(Look::Video(dir))) if open => {
                    let file = copy.unwrap_or_default();
                    let played = cx.background_spawn(async move { files::open_with_system(file) }).await;
                    played.map(|()| Some(Look::Video(dir.clone()))).inspect_err(|_| {
                        files::remove_copies([dir]);
                    })
                }
                Ok(Some(Look::Video(dir))) => {
                    files::remove_copies([dir]);
                    Ok(None)
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

    /// The look is ready (or isn't): it shows and is spent. A photo for a chat left meanwhile (or
    /// none: a video whose player didn't start for it) shows nothing and spends nothing
    /// (`ConversationView.visible`).
    fn looked(
        &mut self,
        key: MessageKey,
        look: Result<Option<Look>, String>,
        generation: u64,
        video: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view_once.opening = None;
        cx.notify();
        let look = match look {
            Ok(None) => return,
            Ok(Some(look @ Look::Photo(_))) if self.generation != generation => {
                let_go(look, cx);
                return;
            }
            Ok(Some(look)) => look,
            Err(e) => {
                let what = if video { "the video" } else { "the photo" };
                self.error = Some(format!("Couldn't open {what}: {e}"));
                return;
            }
        };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.view_once.spent.insert((key.author.clone(), key.sent_at));
        self.view_once.viewer = Some(Viewer { look, focus });
        self.change("mark the view-once media opened", cx, move |client| async move {
            client.view_once_opened(key).await
        });
    }

    /// Close (or Esc): the look goes, and the keys go back to the composer.
    pub(super) fn close_view_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_once.viewer.is_some() {
            self.end_view_once(cx);
            self.composer.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// The look goes (closed, or its chat or screen did): the photo's pixels with it, the video's
    /// copy from the disk. A copy the player still holds (Windows locks it) is said.
    pub(super) fn end_view_once(&mut self, cx: &mut Context<Self>) {
        let Some(viewer) = self.view_once.viewer.take() else {
            return;
        };
        if !let_go(viewer.look, cx) {
            self.error = Some(
                "The view-once video is still on this computer: close it in your video player. Hush removes it when it quits."
                    .into(),
            );
        }
        cx.notify();
    }
}

/// A video's decrypted copy at `file`, in its new folder (removed if it fails); that folder.
async fn video_copy(client: &sig_core::Client, path: String, file: &Path) -> Result<PathBuf, String> {
    let dir = file.parent().ok_or("the copy has no folder")?.to_path_buf();
    let made = match std::fs::create_dir_all(&dir) {
        Ok(()) => files::copy_out(client, path, file, true).await,
        Err(e) => Err(e.to_string()),
    };
    match made {
        Ok(()) => Ok(dir),
        Err(e) => {
            files::remove_copies([dir]);
            Err(e)
        }
    }
}

/// A look's pixels go (once no window is being drawn, as GPUI asks), or its copy; false if the
/// copy couldn't go.
fn let_go(look: Look, cx: &mut App) -> bool {
    match look {
        Look::Photo(photo) => {
            cx.defer(move |cx| cx.drop_image(photo, None));
            true
        }
        Look::Video(dir) => files::remove_copies([dir]),
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s photo, shown over the polls' chat.
    pub(super) fn demo_view_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let photo = files::picture(&crate::demo::picture(), VIEWER_PIXELS).expect("the made-up picture decodes");
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.view_once.viewer = Some(Viewer {
            look: Look::Photo(Arc::new(photo)),
            focus,
        });
    }
}

#[cfg(test)]
mod tests {
    use sig_core::projection::{Attachment, AttachmentState, Message, MessageKey, MessageKind, MessageStatus};

    use super::{label, state, video_copy_path, State};
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
        let data = std::path::Path::new("/data");
        let name = |a: Attachment| {
            let path = video_copy_path(data, &a)?;
            // in a folder of its own among the opened copies, which go at quit and launch
            assert_eq!(path.parent()?.parent()?, data.join(crate::app::files::OPEN_FOLDER));
            Some(path.file_name()?.to_str()?.to_owned())
        };
        // a sender's name could make the player's click run a program or a script
        assert_eq!(name(named("video/mp4", "run.command")).as_deref(), Some("video.mp4"));
        assert_eq!(name(named("video/mp4", "x.app")).as_deref(), Some("video.mp4"));
        assert_eq!(name(named("video/quicktime", "a.exe")).as_deref(), Some("video.mov"));
        assert_eq!(
            name(named("video/webm; codecs=vp9", "a.sh")).as_deref(),
            Some("video.webm")
        );
        // a type that isn't a video this hands to a player: not opened at all
        assert_eq!(name(named("video/x-msdownload", "a.mp4")), None);
        assert_eq!(name(named("application/x-sh", "a.mp4")), None);
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
            cx.bind_keys(crate::key_bindings());
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
        // the chat shortcuts wait for it, and copying takes nothing
        assert!(!hush.read_with(cx, |app, _| app.can_search()));
        cx.simulate_keystrokes(window, "secondary-c");

        act(window, &hush, cx, |app, window, cx| app.close_view_once(window, cx));
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
        assert!(photo.upgrade().is_none(), "its pixels are let go");
        assert!(!hush.read_with(cx, |app, _| app.files.has_picture(&path)));
        assert_eq!(copies(&data), 0);
        assert_eq!(cx.read_from_clipboard(), None);
        // shown once: its button waits, though this core couldn't mark it viewed
        assert_eq!(state(&hush, cx), Some(State::Viewing(file.clone())));
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        assert!(hush.read_with(cx, |app, _| app.view_once.opening.is_none()
            && app.view_once.viewer.is_none()));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_photo_for_a_chat_left_meanwhile_isnt_shown_or_spent(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("left", cx);
        act(window, &hush, cx, |app, window, cx| {
            app.view_once(&file, window, cx);
            // as a switch does
            app.generation += 1;
        });
        until(&hush, cx, |app| app.view_once.opening.is_none());
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
        assert_eq!(hush.read_with(cx, |app, _| app.error.clone()), None, "not spent");
        assert_eq!(state(&hush, cx), Some(State::Open(file.clone())));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_video_handed_to_the_player_is_spent_and_its_copy_goes_with_its_chat(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("handed", cx);
        // as `view_once` hands it over (a test starts no player), after the chat was left
        let dir = data.join(files::OPEN_FOLDER).join("0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("video.mp4"), b"video").unwrap();
        let look = Ok(Some(Look::Video(dir.clone())));
        act(window, &hush, cx, |app, window, cx| {
            let left = app.generation.wrapping_sub(1);
            app.looked(file.message.clone(), look, left, true, window, cx)
        });
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_some()), "shown");
        until(&hush, cx, |app| app.error.is_some());
        let error = hush.update(cx, |app, _| app.error.take()).unwrap();
        assert!(
            error.starts_with("Couldn't mark the view-once media opened: "),
            "spent: {error}"
        );
        // another chat: the look goes, and the copy
        let ana = format!("c:{}", crate::demo::ANA);
        act(window, &hush, cx, |app, window, cx| app.select(ana, window, cx));
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
        assert!(!dir.exists(), "its copy is gone");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_video_that_doesnt_copy_out_isnt_spent_and_leaves_nothing(cx: &mut TestAppContext) {
        let (window, hush, data, file) = open("uncopied", cx);
        let file = Attachment {
            content_type: "video/mp4".into(),
            path: Some(data.join("gone.mp4").to_str().unwrap().into()),
            ..file
        };
        act(window, &hush, cx, |app, window, cx| app.view_once(&file, window, cx));
        until(&hush, cx, |app| app.error.is_some());
        let error = hush.update(cx, |app, _| app.error.take()).unwrap();
        assert!(error.starts_with("Couldn't open the video: "), "{error}");
        assert_eq!(copies(&data), 0, "its folder went");
        assert!(hush.read_with(cx, |app, _| app.view_once.viewer.is_none()));
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
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(hush.read_with(cx, |app, _| app.error.clone()), None, "not spent");
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
        std::fs::write(dir.join("video.mp4"), b"video").unwrap();
        act(window, &hush, cx, |app, _, cx| {
            app.view_once.viewer = Some(super::Viewer {
                look: Look::Video(dir.clone()),
                focus: cx.focus_handle(),
            })
        });
        act(window, &hush, cx, |app, window, cx| app.close_view_once(window, cx));
        assert!(!dir.exists(), "its copy is gone");
        let _ = std::fs::remove_dir_all(&data);
    }
}
