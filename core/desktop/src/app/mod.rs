//! The window: linking, the chat list, a chat's messages and the composer. A subset of the Mac
//! app's rules (`AppStore.swift`); where this one is simpler, a `shortcut:` comment says so.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use async_compat::Compat;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::Theme;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind, Message, MessageKey, TextStyle};
use sig_core::{Client, ConnectionState, CoreError, CoreEvent, EventListener, GroupPreview, HistoryStage};

use crate::{key, text};

mod files;
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

/// One of the New Chat menu's dialogs (the Mac app's sheets in `SidebarView.swift` and
/// `GroupInfo.swift`), with its one text field.
struct Dialog {
    kind: DialogKind,
    /// The username, the group's name or the link.
    field: Entity<InputState>,
    /// New Group's search over the people to add (`PeoplePicker`).
    search: Option<Entity<InputState>>,
    /// Tab and Shift-Tab stay inside it.
    focus: FocusHandle,
    /// Asking the server: the dialog stays until the answer is in.
    busy: bool,
    problem: Option<String>,
}

enum DialogKind {
    Username,
    /// The contacts ticked to be in it.
    Group(HashSet<String>),
    /// The group the link shows, once asked.
    Join(Option<GroupPreview>),
}

/// How a dialog's request ended.
enum Outcome {
    Open(String),
    Preview(GroupPreview),
    Problem(String),
}

/// What the join dialog's button does for the group a link shows (`JoinGroupSheet.primary`).
#[derive(Debug, PartialEq)]
enum JoinStep {
    Open,
    Join,
    AskToJoin,
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
    /// Every chat, archived ones too (a dialog may open one): the sidebar leaves those out.
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
    /// Who a new group can have, by name: (ACI, name).
    contacts: Vec<(String, String)>,
    dialog: Option<Dialog>,
    /// A join request is being withdrawn.
    cancelling: bool,
    files: files::Files,
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
        // opened files' decrypted copies go with the app
        if let Some(dir) = dirs::data_local_dir() {
            files::clear_open_copies(&dir.join(DATA_FOLDER));
        }
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
    // only now: a second copy of Hush, refused the folder, mustn't clear the running one's
    files::clear_open_copies(&data_dir);
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
            contacts: vec![],
            dialog: None,
            cancelling: false,
            files: Default::default(),
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
            crate::Demo::Chat | crate::Demo::NewGroup => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.contacts = addable(&app.chats, app.own_aci.as_deref());
                app.selected = Some("g:01".into());
                app.set_messages(crate::demo::messages(), cx);
                if let crate::Demo::NewGroup = screen {
                    let ana = app.contacts.first().map(|(aci, _)| aci.clone());
                    app.open_dialog(DialogKind::Group(ana.into_iter().collect()), window, cx);
                }
            }
            crate::Demo::Files => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.selected = Some(format!("c:{}", crate::demo::BOGDAN));
                let messages = crate::demo::file_messages();
                app.files = files::Files::demo(crate::demo::files(&messages));
                app.set_messages(messages, cx);
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
            CoreEvent::MessageChanged { conversation_id, key } | CoreEvent::MessageExpired { conversation_id, key } => {
                self.drop_copies_if_gone(&key, cx);
                self.refresh_chats(cx);
                if self.selected.as_deref() == Some(conversation_id.as_str()) {
                    self.reload_messages(cx);
                }
            }
            CoreEvent::AttachmentChanged { conversation_id, .. } => self.attachment_changed(&conversation_id, cx),
            CoreEvent::Error { message } => self.error = Some(message),
            // not in this version: typing, reactions, calls, stories, notifications
            _ => return,
        }
        // a dialog belongs to the chats it was over: linking again doesn't bring it back
        if !matches!(self.screen, Screen::Chats) {
            self.dialog = None;
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
        self.contacts = addable(&all, self.own_aci.as_deref());
        self.chats = all;
        // group rows say who wrote the last message
        let authors = self
            .inbox()
            .filter(|c| c.kind == ConversationKind::Group && !c.last_message_outgoing);
        let mut wanted: Vec<String> = authors.map(|c| c.last_message_author.clone()).collect();
        wanted.extend(self.inbox().flat_map(|c| mentioned(&c.last_message_ranges)));
        self.load_names(wanted, cx);
        self.load_pictures(cx);
        self.mark_read(false, cx);
    }

    /// The chats the sidebar lists: all but the archived ones.
    fn inbox(&self) -> impl Iterator<Item = &Conversation> {
        self.chats.iter().filter(|c| !c.archived)
    }

    /// The chat list's pictures not asked for yet (a new picture is a new file).
    fn load_pictures(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let wanted: Vec<String> = self
            .chats
            .iter()
            .filter(|c| !c.archived)
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
            // from a dialog, whose field had the focus
            self.composer.update(cx, |input, cx| input.focus(window, cx));
            return cx.notify();
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
        self.clear_files(cx);
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
        self.load_files(cx);
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

    fn open_dialog(&mut self, kind: DialogKind, window: &mut Window, cx: &mut Context<Self>) {
        // one still asking the server stays: its outcome must be seen
        if self.dialog.as_ref().is_some_and(|d| d.busy) {
            return;
        }
        let placeholder = match kind {
            DialogKind::Username => "name.42",
            DialogKind::Group(_) => "Group name",
            DialogKind::Join(_) => "Group link",
        };
        let field = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        cx.subscribe_in(&field, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.submit_dialog(window, cx),
            InputEvent::Change => {
                let Some(d) = &mut this.dialog else { return };
                // another link: the group shown isn't the one it joins
                if let DialogKind::Join(preview) = &mut d.kind {
                    *preview = None;
                }
                d.problem = None;
                cx.notify();
            }
            _ => {}
        })
        .detach();
        field.update(cx, |f, cx| f.focus(window, cx));
        let search = matches!(kind, DialogKind::Group(_)).then(|| {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            })
            .detach();
            search
        });
        self.dialog = Some(Dialog {
            kind,
            field,
            search,
            focus: cx.focus_handle(),
            busy: false,
            problem: None,
        });
        cx.notify();
    }

    /// Cancel: not while the server is asked, as its outcome (a group made, say) must be seen.
    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        if self.dialog.as_ref().is_some_and(|d| !d.busy) {
            self.dialog = None;
            cx.notify();
        }
    }

    fn toggle_member(&mut self, aci: String, cx: &mut Context<Self>) {
        if let Some(Dialog {
            kind: DialogKind::Group(chosen),
            ..
        }) = &mut self.dialog
        {
            if !chosen.remove(&aci) {
                chosen.insert(aci);
            }
            cx.notify();
        }
    }

    /// The dialog's default button: Find, Create, Show Group, then Join (or Open Chat).
    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let listed = |id: &str| self.chats.iter().any(|c| c.id == id);
        let Some(d) = &self.dialog else { return };
        let asked = d.field.read(cx).value().trim().to_string();
        if d.busy || asked.is_empty() {
            return;
        }
        let value = asked.clone();
        let work: LocalBoxFuture<Outcome> = match &d.kind {
            DialogKind::Username => async move {
                match client.find_username(value).await {
                    Ok(Some(id)) => Outcome::Open(id),
                    Ok(None) => Outcome::Problem("No one has this username.".into()),
                    Err(e) => Outcome::Problem(describe(&e)),
                }
            }
            .boxed_local(),
            DialogKind::Group(chosen) => {
                let members = self.contacts.iter().map(|(aci, _)| aci).filter(|a| chosen.contains(*a));
                let members: Vec<String> = members.cloned().collect();
                async move {
                    match client.create_group(value, members).await {
                        Ok(id) => Outcome::Open(id),
                        Err(e) => Outcome::Problem(server_failure("Couldn't make the group", &e)),
                    }
                }
                .boxed_local()
            }
            DialogKind::Join(None) => async move {
                match client.group_link_preview(value).await {
                    Ok(preview) => Outcome::Preview(preview),
                    Err(e) => Outcome::Problem(server_failure("Couldn't read the link", &e)),
                }
            }
            .boxed_local(),
            DialogKind::Join(Some(preview)) => {
                if join_step(preview, listed(&preview.conversation_id)) == JoinStep::Open {
                    let id = preview.conversation_id.clone();
                    self.dialog = None;
                    return self.select(id, window, cx);
                }
                async move {
                    match client.join_group(value).await {
                        Ok(joined) => Outcome::Open(joined.conversation_id),
                        Err(e) => Outcome::Problem(server_failure("Couldn't join the group", &e)),
                    }
                }
                .boxed_local()
            }
        };
        if let Some(d) = &mut self.dialog {
            d.busy = true;
            d.problem = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = Compat::new(work).await;
            let _ = this.update_in(cx, |app, window, cx| app.dialog_done(asked, outcome, window, cx));
        })
        .detach();
    }

    fn dialog_done(&mut self, asked: String, outcome: Outcome, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dialog else { return };
        d.busy = false;
        // the text changed meanwhile: a preview or a problem isn't about what it says now
        let same = d.field.read(cx).value().trim() == asked;
        match outcome {
            Outcome::Open(id) => {
                self.dialog = None;
                self.select(id, window, cx);
                self.refresh_chats(cx);
            }
            Outcome::Preview(p) if same => {
                if let DialogKind::Join(preview) = &mut d.kind {
                    *preview = Some(p);
                }
            }
            Outcome::Problem(why) if same => d.problem = Some(why),
            Outcome::Preview(_) | Outcome::Problem(_) => {}
        }
        cx.notify();
    }

    /// Withdraws our request to join the group, then deletes its chat; a failure is said (the
    /// chat then stays).
    fn cancel_join_request(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.cancelling {
            return;
        }
        self.cancelling = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let cancelled = Compat::new(client.cancel_join_request(id.clone())).await;
            let _ = this.update(cx, |app, cx| {
                app.cancelling = false;
                match cancelled {
                    Ok(()) => app.delete_chat(id, cx),
                    Err(e) => app.error = Some(server_failure("Couldn't cancel the request", &e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Deletes the chat's messages here (a person's on the phone too); it closes
    /// (`AppStore.deleteChat`). A failure is said.
    fn delete_chat(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.selected.as_ref() == Some(&id) {
            self.selected = None;
            self.generation += 1;
            self.messages.clear();
            self.clear_files(cx);
            self.timeline.update(cx, |t, cx| t.reset(0, cx));
        }
        self.drop_chat_copies(&id);
        self.drafts.remove(&id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let Err(e) = Compat::new(client.delete_chat(id)).await else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                app.error = Some(format!("Couldn't delete the chat: {}", describe(&e)));
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

/// Who a new group can have: the people with a chat here, but strangers, blocked ones and us; by
/// name (`AppStore.addableContacts`).
fn addable(all: &[Conversation], own_aci: Option<&str>) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = all
        .iter()
        .filter(|c| c.kind == ConversationKind::Direct && !c.blocked && !c.message_request)
        .filter_map(|c| {
            let aci =
                c.id.strip_prefix("c:")
                    .filter(|a| !a.starts_with("PNI:") && Some(*a) != own_aci)?;
            Some((aci.to_owned(), text::title(c, own_aci)))
        })
        .collect();
    found.sort_by_key(|(_, name)| name.to_lowercase());
    found
}

/// The people (ACI, name) whose name has `filter` in it, in any case (`PeoplePicker`).
fn matching<'a>(people: &'a [(String, String)], filter: &str) -> impl Iterator<Item = &'a (String, String)> {
    let filter = filter.trim().to_lowercase();
    people
        .iter()
        .filter(move |(_, name)| name.to_lowercase().contains(&filter))
}

fn join_step(p: &GroupPreview, listed: bool) -> JoinStep {
    // no chat here (in it with no messages yet, or asked from another device): joining or asking
    // again lists it; the Mac app's Open Chat would open nothing
    if listed && (p.member || p.requested) {
        JoinStep::Open
    } else if p.approval {
        JoinStep::AskToJoin
    } else {
        JoinStep::Join
    }
}

/// A failure as the Mac app says it (`AppStore.describe`).
fn describe(e: &CoreError) -> String {
    match e {
        CoreError::Storage { message } => format!("Hush couldn't open its data: {message}"),
        CoreError::InvalidInput { message } => message.clone(),
        CoreError::Network { message } => format!("The server couldn't be reached: {message}"),
        e => e.to_string(),
    }
}

/// A server's refusal, after what was tried (`AppStore.serverFailure`).
fn server_failure(what: &str, e: &CoreError) -> String {
    match e {
        CoreError::Network { message } => format!("{what}: {message}"),
        e => format!("{what}: {}", describe(e)),
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
    use super::{addable, changed_rows, describe, join_step, matching, JoinStep};
    use sig_core::projection::{Conversation, ConversationKind, MessageKind, MessageStatus};
    use sig_core::{CoreError, GroupPreview};

    fn chat(id: &str, title: &str) -> Conversation {
        Conversation {
            id: id.into(),
            title: title.into(),
            kind: if id.starts_with("g:") {
                ConversationKind::Group
            } else {
                ConversationKind::Direct
            },
            last_message_at: 0,
            last_message_preview: String::new(),
            last_message_ranges: vec![],
            unread_count: 0,
            last_message_author: String::new(),
            last_message_outgoing: false,
            last_message_status: MessageStatus::Read,
            last_message_deleted: false,
            last_message_file: None,
            expire_timer: 0,
            last_message_kind: MessageKind::Normal,
            last_message_undecryptable: false,
            archived: false,
            pinned_order: None,
            mute_until: 0,
            marked_unread: false,
            blocked: false,
            whitelisted: true,
            message_request: false,
            group_invitation: false,
            group_join_requested: false,
            avatar: None,
            username: None,
            note: None,
            chat_color: None,
            wallpaper: None,
        }
    }

    #[test]
    fn a_new_group_can_have_the_people_with_a_chat_here() {
        let blocked = Conversation {
            blocked: true,
            ..chat("c:b", "Blocked")
        };
        let stranger = Conversation {
            message_request: true,
            ..chat("c:s", "Stranger")
        };
        let all = [
            chat("c:z", "zoë"),
            chat("g:1", "A group"),
            chat("c:me", "Me"),
            chat("c:PNI:p", "By number"),
            blocked,
            stranger,
            chat("c:a", "Ana"),
        ];
        let names: Vec<_> = addable(&all, Some("me")).into_iter().map(|(aci, _)| aci).collect();
        assert_eq!(names, ["a", "z"]);
    }

    #[test]
    fn the_join_button_opens_joins_or_asks() {
        let group = GroupPreview {
            conversation_id: "g:1".into(),
            title: "Hikers".into(),
            description: String::new(),
            member_count: 3,
            approval: false,
            requested: false,
            member: false,
        };
        assert_eq!(join_step(&group, false), JoinStep::Join);
        let approval = GroupPreview {
            approval: true,
            ..group.clone()
        };
        assert_eq!(join_step(&approval, false), JoinStep::AskToJoin);
        let member = GroupPreview {
            member: true,
            ..group.clone()
        };
        assert_eq!(join_step(&member, true), JoinStep::Open);
        // in it, but its chat isn't listed here (no messages yet): joining lists it
        assert_eq!(join_step(&member, false), JoinStep::Join);
        // asked here: its chat opens; asked elsewhere: asking again lists it
        let requested = GroupPreview {
            requested: true,
            ..approval
        };
        assert_eq!(join_step(&requested, true), JoinStep::Open);
        assert_eq!(join_step(&requested, false), JoinStep::AskToJoin);
    }

    #[test]
    fn the_people_search_finds_names_in_any_case() {
        let people: Vec<(String, String)> = [("a", "Ana Pop"), ("b", "Bogdan"), ("c", "Ioana")]
            .map(|(aci, name)| (aci.to_owned(), name.to_owned()))
            .into();
        let found = |filter: &str| -> Vec<&str> { matching(&people, filter).map(|(aci, _)| aci.as_str()).collect() };
        assert_eq!(found(""), ["a", "b", "c"]);
        assert_eq!(found(" ANA "), ["a", "c"]);
        assert_eq!(found("pop"), ["a"]);
        assert!(found("zoë").is_empty());
    }

    #[test]
    fn a_failure_is_said_as_the_mac_app_says_it() {
        let storage = CoreError::Storage {
            message: "disk full".into(),
        };
        assert_eq!(describe(&storage), "Hush couldn't open its data: disk full");
        let network = CoreError::Network {
            message: "timed out".into(),
        };
        assert_eq!(describe(&network), "The server couldn't be reached: timed out");
    }

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
