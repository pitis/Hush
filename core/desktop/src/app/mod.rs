//! The window: linking, the chat list, a chat's messages and the composer. A subset of the Mac
//! app's rules (`AppStore.swift`); where this one is simpler, a `shortcut:` comment says so.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use async_compat::Compat;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::Theme;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind, Message, MessageKey, TextStyle};
use sig_core::{Client, ConnectionState, CoreEvent, EventListener, HistoryStage};

use crate::{key, text};

mod view;

/// How the phone lists this computer.
const DEVICE_NAME: &str = "Hush Desktop";
/// Inside the OS's local app data folder (not Windows' roaming one: this device's keys stay on this
/// computer); not the Mac app's "Hush", so the two never share data.
const DATA_FOLDER: &str = "Hush Desktop";
/// shortcut: only a chat's newest messages are loaded; page older ones with
/// `Client::messages(id, before, limit)` once scrolling back is wanted.
const PAGE: u32 = 100;

enum Screen {
    Opening,
    Link {
        url: Option<String>,
    },
    Chats,
    /// Removed on the phone while in use.
    Unlinked,
    Failed(String),
}

pub struct Hush {
    client: Option<Arc<Client>>,
    data_dir: PathBuf,
    own_aci: Option<String>,
    screen: Screen,
    connection: ConnectionState,
    error: Option<String>,
    /// The phone's message history on its way (stage, done, total), or a failed transfer:
    /// receiving waits for it, so it is shown with its ways out.
    history: Option<(HistoryStage, u64, u64)>,
    /// The chat list: archived chats are left out.
    chats: Vec<Conversation>,
    chats_loading: bool,
    chats_again: bool,
    selected: Option<String>,
    /// The open chat's newest messages, oldest first.
    messages: Vec<Message>,
    /// Bumped on every selection, so a load for the chat open before is dropped.
    generation: u64,
    messages_loading: Option<u64>,
    messages_again: bool,
    /// Contacts' names by ACI.
    names: HashMap<String, String>,
    /// ACIs whose name was asked for (asked again only after a failed lookup).
    asked: HashSet<String>,
    /// Chats' pictures by their file, decrypted by the core (they are encrypted on disk); None
    /// while asked for, or one that isn't a picture. One that didn't read is asked for again.
    pictures: HashMap<String, Option<Arc<Image>>>,
    /// Unsent text of the chats that aren't open.
    /// shortcut: kept only while the app runs, and not shown in the chat list; the Mac app keeps
    /// them and shows "Draft: …".
    drafts: HashMap<String, String>,
    /// The chat whose `mark_read` is running, so a refresh in between doesn't start another.
    marking_read: Option<String>,
    window_active: bool,
    /// Not behind other windows, minimized or on a sleeping or locked display.
    window_visible: bool,
    composer: Entity<TextareaState>,
    /// Nothing to send: the send button hides.
    composer_empty: bool,
    /// The sidebar's search field and its words (chats whose name or last message has them).
    search: Entity<InputState>,
    query: String,
    timeline: Entity<MessageScrollerState>,
}

/// The open core, for `quit`.
struct Core(Arc<Client>);

impl Global for Core {}

/// Quits once the core has stopped (a few seconds at most): it first handles what it has already
/// received, which the server counts as delivered and never sends again.
pub fn quit(cx: &mut App) {
    let Some(Core(client)) = cx.try_global::<Core>() else {
        return cx.quit();
    };
    let client = client.clone();
    cx.spawn(async move |cx| {
        Compat::new(client.stop()).await;
        cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Listener(UnboundedSender<CoreEvent>);

impl EventListener for Listener {
    fn on_event(&self, event: CoreEvent) {
        // the window is gone: nothing is left to tell
        let _ = self.0.unbounded_send(event);
    }
}

/// The core over this computer's data folder, and that folder.
fn open(events: UnboundedSender<CoreEvent>) -> Result<(Arc<Client>, PathBuf), String> {
    let data_dir = dirs::data_local_dir()
        .ok_or("this system has no folder for app data")?
        .join(DATA_FOLDER);
    let dir = data_dir
        .to_str()
        .ok_or("the data folder's path isn't valid text")?
        .to_owned();
    let key = key::db_key()?;
    let client =
        Client::open(dir, key, Arc::new(Listener(events))).map_err(|e| format!("couldn't open the data: {e}"))?;
    Ok((client, data_dir))
}

impl Hush {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Theme::sync_system_appearance(Some(window), cx);
        cx.observe_window_appearance(window, |_, window, cx| Theme::sync_system_appearance(Some(window), cx))
            .detach();
        let app = Self::window(window, cx);

        let (events, mut incoming) = unbounded();
        cx.spawn(async move |this, cx| {
            while let Some(event) = incoming.next().await {
                if this.update(cx, |app, cx| app.handle(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // off the main thread: the keychain may ask, and the store opens on disk
        let opening = cx.background_spawn(async move { open(events) });
        cx.spawn(async move |this, cx| {
            let client = opening.await;
            this.update(cx, |app, cx| app.opened(client, cx))
        })
        .detach();
        app
    }

    /// The window's state and controls, before any core.
    fn window(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.mark_read(false, cx);
        })
        .detach();
        // the active window stays active under a locked screen: being seen is a separate test
        cx.observe_window_visibility(window, |this, _, window, cx| {
            this.window_visible = window.is_visible();
            this.mark_read(false, cx);
        })
        .detach();

        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Message")
                .submit_on_enter(true)
        });
        cx.subscribe_in(
            &composer,
            window,
            |this, composer, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                InputEvent::Change => {
                    this.composer_empty = composer.read(cx).value().trim().is_empty();
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        cx.subscribe(&search, |this, search, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.query = search.read(cx).value().trim().to_lowercase();
                cx.notify();
            }
        })
        .detach();
        let timeline = cx.new(|cx| MessageScrollerState::new(0, cx));
        // scrolling down to the newest message reads it
        cx.observe(&timeline, |this, _, cx| this.mark_read(false, cx)).detach();

        Self {
            client: None,
            data_dir: PathBuf::new(),
            own_aci: None,
            screen: Screen::Opening,
            connection: ConnectionState::Connecting,
            error: None,
            history: None,
            chats: vec![],
            chats_loading: false,
            chats_again: false,
            selected: None,
            messages: vec![],
            generation: 0,
            messages_loading: None,
            messages_again: false,
            names: HashMap::new(),
            asked: HashSet::new(),
            pictures: HashMap::new(),
            drafts: HashMap::new(),
            marking_read: None,
            window_active: window.is_window_active(),
            window_visible: window.is_visible(),
            composer,
            composer_empty: true,
            search,
            query: String::new(),
            timeline,
        }
    }

    /// A screen of made-up chats for `--snapshot`: no core, nobody's real messages.
    #[cfg(feature = "snapshot")]
    pub fn demo(screen: crate::Demo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut app = Self::window(window, cx);
        app.own_aci = Some(crate::demo::ME.into());
        app.names.extend(crate::demo::names());
        app.connection = ConnectionState::Online;
        match screen {
            crate::Demo::Link => {
                app.screen = Screen::Link {
                    url: Some("sgnl://linkdevice?uuid=demo&pub_key=demo".into()),
                }
            }
            crate::Demo::Empty => app.screen = Screen::Chats,
            crate::Demo::Chat => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.selected = Some("g:01".into());
                app.set_messages(crate::demo::messages(), cx);
            }
        }
        app
    }

    /// Starts receiving, or linking when this computer isn't linked yet.
    fn opened(&mut self, opened: Result<(Arc<Client>, PathBuf), String>, cx: &mut Context<Self>) {
        let client = match opened {
            Ok((client, data_dir)) => {
                self.data_dir = data_dir;
                client
            }
            Err(e) => {
                self.screen = Screen::Failed(e);
                return cx.notify();
            }
        };
        let linked = client.is_linked();
        if linked {
            self.own_aci = client.own_aci();
            self.screen = Screen::Chats;
            self.start(client.clone(), cx);
        } else {
            self.screen = Screen::Link { url: None };
            if let Err(e) = client.link(DEVICE_NAME.into()) {
                self.screen = Screen::Failed(e.to_string());
            }
        }
        cx.set_global(Core(client.clone()));
        self.client = Some(client);
        // an unlinked core has no chats yet: its `Ready` after linking loads them
        if linked {
            self.refresh_chats(cx);
        }
        cx.notify();
    }

    /// Starts receiving once it's known whether a history transfer is still pending: checked
    /// first, so a fast import can't finish before its progress shows and leave it up.
    fn start(&mut self, client: Arc<Client>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            // unknown: the core checks again when starting, and reports a failure itself
            let pending = Compat::new(client.history_pending()).await.unwrap_or(false);
            let _ = this.update(cx, |app, cx| {
                if pending && app.history.is_none() {
                    app.history = Some((HistoryStage::Waiting, 0, 0));
                }
                if let Err(e) = client.start() {
                    app.error = Some(format!("Couldn't start receiving: {e}"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn handle(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::LinkUrl { url } => {
                self.error = None;
                self.screen = Screen::Link { url: Some(url) };
            }
            // an error that came with linking (the history key not kept, say) stays shown
            CoreEvent::Linked => {
                self.screen = Screen::Chats;
                if let Some(client) = self.client.clone() {
                    self.own_aci = client.own_aci();
                    self.start(client, cx);
                }
            }
            CoreEvent::Connection { state } => {
                let linked = self.client.as_ref().is_some_and(|c| c.is_linked());
                if state == ConnectionState::Unlinked {
                    // removed on the phone while in use; otherwise a linking attempt failed
                    self.screen = match self.screen {
                        Screen::Chats => Screen::Unlinked,
                        _ => Screen::Link { url: None },
                    };
                } else if linked && !matches!(self.screen, Screen::Chats) {
                    self.screen = Screen::Chats;
                }
                self.connection = state;
            }
            // a chat event can be a message that couldn't be decrypted: the list says so
            CoreEvent::Ready | CoreEvent::ConversationsChanged | CoreEvent::ChatEventAdded { .. } => {
                self.refresh_chats(cx)
            }
            CoreEvent::HistoryProgress { stage, done, total } => match stage {
                HistoryStage::Done => {
                    self.history = None;
                    self.refresh_chats(cx);
                }
                HistoryStage::NoHistory => self.history = None,
                stage => self.history = Some((stage, done, total)),
            },
            CoreEvent::MessageChanged { conversation_id, .. } | CoreEvent::MessageExpired { conversation_id, .. } => {
                self.refresh_chats(cx);
                if self.selected.as_deref() == Some(conversation_id.as_str()) {
                    self.reload_messages(cx);
                }
            }
            CoreEvent::Error { message } => self.error = Some(message),
            // not in this version: typing, reactions, calls, stories, files, notifications
            _ => return,
        }
        cx.notify();
    }

    /// Reloads the chat list; calls while a load runs are folded into one more.
    fn refresh_chats(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.chats_loading {
            self.chats_again = true;
            return;
        }
        self.chats_loading = true;
        cx.spawn(async move |this, cx| loop {
            let found = Compat::new(client.conversations()).await;
            let again = this.update(cx, |app, cx| {
                match found {
                    Ok(found) => app.set_chats(found, cx),
                    Err(e) => app.error = Some(format!("Couldn't load the chats: {e}")),
                }
                app.chats_loading = std::mem::take(&mut app.chats_again);
                cx.notify();
                app.chats_loading
            });
            if !again.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    fn set_chats(&mut self, all: Vec<Conversation>, cx: &mut Context<Self>) {
        for c in &all {
            if let (ConversationKind::Direct, Some(aci)) = (c.kind, c.id.strip_prefix("c:")) {
                if !c.title.is_empty() {
                    self.names.insert(aci.to_owned(), c.title.clone());
                }
            }
        }
        self.chats = all.into_iter().filter(|c| !c.archived).collect();
        // group rows say who wrote the last message
        let authors = self
            .chats
            .iter()
            .filter(|c| c.kind == ConversationKind::Group && !c.last_message_outgoing);
        let mut wanted: Vec<String> = authors.map(|c| c.last_message_author.clone()).collect();
        wanted.extend(self.chats.iter().flat_map(|c| mentioned(&c.last_message_ranges)));
        self.load_names(wanted, cx);
        self.load_pictures(cx);
        self.mark_read(false, cx);
    }

    /// The chat list's pictures not asked for yet (a new picture is a new file).
    fn load_pictures(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let wanted: Vec<String> = self
            .chats
            .iter()
            .filter_map(|c| c.avatar.clone())
            .filter(|path| {
                let new = !self.pictures.contains_key(path);
                if new {
                    self.pictures.insert(path.clone(), None);
                }
                new
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for path in wanted {
                let bytes = Compat::new(client.file_bytes(path.clone())).await;
                let shown = this.update(cx, |app, cx| match bytes {
                    Ok(b) => {
                        let image = image_format(&b).map(|f| Arc::new(Image::from_bytes(f, b)));
                        app.pictures.insert(path, image);
                        cx.notify();
                    }
                    // the core logs why; the next refresh asks again
                    Err(_) => {
                        app.pictures.remove(&path);
                    }
                });
                if shown.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// The decrypted picture of a chat whose picture is `path`, once read.
    pub(crate) fn picture(&self, path: &str) -> Option<Arc<Image>> {
        self.pictures.get(path).cloned().flatten()
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        let draft = self.drafts.remove(&id).unwrap_or_default();
        let unsent = self.composer.read(cx).value().to_string();
        if let Some(left) = self.selected.replace(id) {
            if !unsent.trim().is_empty() {
                self.drafts.insert(left, unsent);
            }
        }
        self.composer_empty = draft.trim().is_empty();
        self.composer.update(cx, |input, cx| {
            input.set_value(draft, window, cx);
            input.focus(window, cx);
        });
        self.generation += 1;
        self.messages.clear();
        self.timeline.update(cx, |t, cx| t.reset(0, cx));
        self.reload_messages(cx);
        self.mark_read(true, cx);
        cx.notify();
    }

    /// Reloads the open chat's newest messages; calls while a load runs are folded into one more.
    fn reload_messages(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        let generation = self.generation;
        if self.messages_loading == Some(generation) {
            self.messages_again = true;
            return;
        }
        self.messages_loading = Some(generation);
        self.messages_again = false;
        cx.spawn(async move |this, cx| loop {
            let page = Compat::new(client.messages(id.clone(), None, PAGE)).await;
            let again = this.update(cx, |app, cx| {
                if app.generation != generation {
                    return false; // another chat was opened meanwhile
                }
                match page {
                    Ok(page) => app.set_messages(page, cx),
                    Err(e) => app.error = Some(format!("Couldn't load the messages: {e}")),
                }
                let again = std::mem::take(&mut app.messages_again);
                if !again {
                    app.messages_loading = None;
                }
                cx.notify();
                again
            });
            if !again.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    fn set_messages(&mut self, page: Vec<Message>, cx: &mut Context<Self>) {
        let old: Vec<MessageKey> = self.messages.iter().map(Message::key).collect();
        let new: Vec<MessageKey> = page.iter().map(Message::key).collect();
        self.messages = page;
        let (range, count) = changed_rows(&old, &new);
        // only rows that came or went are replaced, so a reader scrolled up stays put; with the
        // same messages, some changed (a tick, an edit) and their heights may have too
        self.timeline.update(cx, |t, cx| {
            if range.is_empty() && count == 0 {
                t.remeasure(cx);
            } else {
                t.splice(range, count, cx);
            }
        });
        let mut wanted: Vec<String> = self
            .messages
            .iter()
            .filter(|m| !m.is_outgoing)
            .map(|m| m.author.clone())
            .collect();
        wanted.extend(self.messages.iter().flat_map(|m| mentioned(&m.ranges)));
        self.load_names(wanted, cx);
    }

    /// Looks up each unknown contact's name; a failed lookup is tried again next time.
    fn load_names(&mut self, acis: Vec<String>, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let wanted: Vec<String> = acis
            .into_iter()
            .filter(|aci| !aci.is_empty() && !self.names.contains_key(aci) && self.asked.insert(aci.clone()))
            .collect();
        if wanted.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for aci in wanted {
                let found = Compat::new(client.title(format!("c:{aci}"))).await;
                let answered = this.update(cx, |app, cx| match found {
                    Ok(Some(name)) => {
                        app.names.insert(aci, name);
                        cx.notify();
                    }
                    // no name yet: the short id stays
                    Ok(None) => {}
                    Err(_) => {
                        app.asked.remove(&aci);
                    }
                });
                if answered.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// The open chat is read once its newest message is on screen in the active, visible window
    /// (the Mac app's `ChatList.shouldMarkRead`). A chat marked unread by hand counts only when it
    /// is opened, so marking it unread on the phone isn't undone here at once.
    fn mark_read(&mut self, opening: bool, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        let Some(c) = self.chats.iter().find(|c| c.id == id) else {
            return;
        };
        let unread = c.unread_count > 0 || (opening && c.marked_unread);
        let at_bottom = !self.timeline.read(cx).is_scrolled_up();
        let seen = self.window_active && self.window_visible && at_bottom;
        if !unread || !seen || self.marking_read.as_ref() == Some(&id) {
            return;
        }
        self.marking_read = Some(id.clone());
        cx.spawn(async move |this, cx| {
            let marked = Compat::new(client.mark_read(id.clone())).await;
            let _ = this.update(cx, |app, cx| {
                if app.marking_read.as_ref() == Some(&id) {
                    app.marking_read = None;
                }
                if let Err(e) = marked {
                    app.error = Some(format!("Couldn't mark the chat read: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        let body = self.composer.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        self.composer.update(cx, |input, cx| input.clean(window, cx));
        cx.spawn_in(window, async move |this, cx| {
            let sent = Compat::new(client.send_text(id.clone(), body.clone(), vec![], None, None)).await;
            let Err(e) = sent else { return };
            let _ = this.update_in(cx, |app, window, cx| {
                app.error = Some(format!("Couldn't send the message: {e}"));
                // the text goes back to its chat: into the composer if that chat is still open
                // and nothing new was typed, else into its draft
                if app.selected.as_deref() == Some(id.as_str()) && app.composer.read(cx).value().is_empty() {
                    app.composer_empty = false;
                    app.composer.update(cx, |input, cx| input.set_value(body, window, cx));
                } else {
                    app.drafts.entry(id).or_insert(body);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// After a failed history transfer: picks it up where it stopped.
    fn retry_history(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        let before = self.history.replace((HistoryStage::Waiting, 0, 0));
        if let Err(e) = client.retry_history() {
            self.history = before;
            self.error = Some(format!("Couldn't retry the transfer: {e}"));
        }
        cx.notify();
    }

    /// "Continue without history": receiving starts; the core says `NoHistory`.
    fn cancel_history(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        if let Err(e) = client.cancel_history() {
            self.error = Some(format!("Couldn't stop the transfer: {e}"));
        }
        cx.notify();
    }

    /// Receiving may have stopped (it restarts only on request): starting is idempotent.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        if let Err(e) = client.start().and_then(|()| client.reconnect()) {
            self.error = Some(format!("Couldn't reconnect: {e}"));
        }
        cx.notify();
    }

    fn retry_link(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        self.error = None;
        if let Err(e) = client.link(DEVICE_NAME.into()) {
            self.error = Some(e.to_string());
        }
        cx.notify();
    }

    /// A contact's name, else a short id; "You" for us.
    fn name(&self, aci: &str) -> String {
        if self.own_aci.as_deref() == Some(aci) {
            return "You".into();
        }
        self.names.get(aci).cloned().unwrap_or_else(|| text::short_id(aci))
    }
}

/// The rows that changed between two lists of keys: `old[range]` became `count` rows of `new`
/// (what both share at the start and at the end stays).
fn changed_rows<K: PartialEq>(old: &[K], new: &[K]) -> (Range<usize>, usize) {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (start..old.len() - end, new.len() - start - end)
}

/// The people a message's text mentions.
fn mentioned(ranges: &[sig_core::projection::BodyRange]) -> impl Iterator<Item = String> + '_ {
    ranges.iter().filter_map(|r| match &r.style {
        TextStyle::Mention { aci } => Some(aci.clone()),
        _ => None,
    })
}

/// A picture's format from its first bytes, of those GPUI decodes from memory (as `img` of a
/// file guessed them; chat pictures are JPEG, PNG, GIF or WebP).
fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some(ImageFormat::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [b'G', b'I', b'F', b'8', ..] => Some(ImageFormat::Gif),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::Webp),
        [b'B', b'M', ..] => Some(ImageFormat::Bmp),
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => Some(ImageFormat::Tiff),
        [0, 0, 1, 0, ..] => Some(ImageFormat::Ico),
        [b'P', b'1'..=b'7', ..] => Some(ImageFormat::Pnm),
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_pictures_format_is_told_by_its_first_bytes() {
        use super::{image_format, ImageFormat};
        assert_eq!(image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageFormat::Jpeg));
        assert_eq!(image_format(b"\x89PNG\r\n"), Some(ImageFormat::Png));
        assert_eq!(image_format(b"GIF89a"), Some(ImageFormat::Gif));
        assert_eq!(image_format(b"RIFF\0\0\0\0WEBPVP8 "), Some(ImageFormat::Webp));
        assert_eq!(image_format(b"BM6\0"), Some(ImageFormat::Bmp));
        assert_eq!(image_format(b"II*\0"), Some(ImageFormat::Tiff));
        assert_eq!(image_format(b"MM\0*"), Some(ImageFormat::Tiff));
        assert_eq!(image_format(&[0, 0, 1, 0, 1, 0]), Some(ImageFormat::Ico));
        assert_eq!(image_format(b"P6\n"), Some(ImageFormat::Pnm));
        assert_eq!(image_format(b"HUSHFIL1"), None, "still encrypted: nothing to show");
        assert_eq!(image_format(b""), None);
    }
    use super::changed_rows;

    #[test]
    fn only_the_rows_that_came_or_went_change() {
        // a new message
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 2, 3, 4]), (3..3, 1));
        // the window moved on by one: the oldest went, a new one came
        assert_eq!(changed_rows(&[1, 2, 3], &[2, 3, 4]), (0..3, 3));
        // one expired in the middle
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 3]), (1..2, 0));
        // nothing came or went
        assert_eq!(changed_rows(&[1, 2], &[1, 2]), (2..2, 0));
        // the first load
        assert_eq!(changed_rows(&[], &[1, 2]), (0..0, 2));
        // a key both at the start and the end is counted once
        assert_eq!(changed_rows(&[1, 2, 1], &[1]), (1..3, 0));
    }
}
