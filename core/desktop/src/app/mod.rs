//! The window: linking, the chat list, a chat's messages and the composer. A subset of the Mac
//! app's rules (`AppStore.swift`); where this one is simpler, a `shortcut:` comment says so.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use async_compat::Compat;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt};
use gpui_kit::component::input::{InputEditsEvent, InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::Theme;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind, Message, TextStyle};
use sig_core::{Client, ConnectionState, CoreError, CoreEvent, EventListener, GroupPreview, HistoryStage};

use crate::{key, text};

mod actions;
pub mod attach;
mod chats;
pub mod compose;
mod drafts;
mod files;
pub mod navigate;
mod paging;
mod previews;
pub mod search;
mod view;

/// How the phone lists this computer.
const DEVICE_NAME: &str = "Hush Desktop";
/// Inside the OS's local app data folder (not Windows' roaming one: this device's keys stay on this
/// computer); not the Mac app's "Hush", so the two never share data.
const DATA_FOLDER: &str = "Hush Desktop";
/// A chat's newest messages load, a page more each time the timeline is scrolled up to the oldest.
/// shortcut: each load reads them all again, with their reactions and files, back to the oldest
/// shown; merge pages read with `Client::messages(id, before, limit)` (`AppStore.merge`) if a long
/// scroll back gets slow.
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
    /// In it, with no chat here (no messages since linking): listed first, then opened.
    List,
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
    /// Every chat, archived ones too: the sidebar lists those in its archive.
    chats: Vec<Conversation>,
    chats_loading: bool,
    chats_again: bool,
    selected: Option<String>,
    /// The open chat's newest messages, oldest first.
    messages: Vec<Message>,
    /// Bumped on every selection, so a load for the chat open before is dropped.
    generation: u64,
    /// How many of the open chat's newest messages load: `PAGE`, more for the older pages scrolled
    /// to, or back to a found message.
    depth: u32,
    messages_loading: Option<u64>,
    messages_again: bool,
    /// Contacts' names by ACI.
    names: HashMap<String, String>,
    /// ACIs whose name was asked for (asked again only after a failed lookup).
    asked: HashSet<String>,
    /// Chats' pictures by their file, decrypted by the core (they are encrypted on disk); None
    /// while asked for, or one that isn't a picture. One that didn't read is asked for again.
    pictures: HashMap<String, Option<Arc<Image>>>,
    /// Unsent text, kept by the core: a chat that isn't open shows "Draft: …".
    drafts: drafts::Drafts,
    /// The chat whose `mark_read` is running, so a refresh in between doesn't start another.
    marking_read: Option<String>,
    window_active: bool,
    /// Not behind other windows, minimized or on a sleeping or locked display.
    window_visible: bool,
    composer: Entity<TextareaState>,
    /// Nothing to send: the send button hides.
    composer_empty: bool,
    /// Files to send with the next message.
    tray: Vec<attach::Outgoing>,
    /// Files being added to the tray (read, a pasted picture written).
    adding: usize,
    /// ↩ was pressed while files were being added: the message goes once they're in.
    send_waiting: bool,
    /// The sidebar's search field and its words (chats whose name has them; messages in `find`).
    search: Entity<InputState>,
    query: String,
    /// The messages the search found.
    find: search::Find,
    timeline: Entity<MessageScrollerState>,
    /// Who a new group can have, by name: (ACI, name).
    contacts: Vec<(String, String)>,
    dialog: Option<Dialog>,
    /// A join request is being withdrawn, or an invitation answered.
    answering: bool,
    /// The archive, and the chat changes on their way.
    chat_actions: chats::ChatActions,
    /// The open chat's reactions, and the reply or edit in the composer.
    actions: actions::Actions,
    files: files::Files,
    /// Older pages on their way, and messages come in while scrolled up.
    paging: paging::Paging,
    /// The composer's styles and mentions.
    compose: compose::Compose,
    previews: previews::Previews,
}

/// The open core and the draft writes on their way, for `quit`.
struct Core(Arc<Client>, drafts::Writer);

impl Global for Core {}

/// Quits once the drafts typed are written and the core has stopped (a few seconds at most): it
/// first handles what it has already received, which the server counts as delivered and never
/// sends again.
pub fn quit(cx: &mut App) {
    let Some(Core(client, drafts)) = cx.try_global::<Core>() else {
        return cx.quit();
    };
    let (client, written) = (client.clone(), drafts.finish());
    cx.spawn(async move |cx| {
        // the last thing typed is kept
        written.await;
        Compat::new(client.stop()).await;
        // opened files' decrypted copies go with the app, and pasted pictures
        if let Some(dir) = dirs::data_local_dir() {
            files::clear_open_copies(&dir.join(DATA_FOLDER));
            attach::clear_pasted(&dir.join(DATA_FOLDER));
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
    attach::clear_pasted(&data_dir);
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
                    this.composer_changed(cx);
                    this.keep_draft(cx);
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        cx.subscribe(&composer, |this, _, e: &InputEditsEvent, cx| {
            this.composer_edited(e, cx)
        })
        .detach();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search").clean_on_escape());
        cx.subscribe_in(
            &search,
            window,
            |this, search, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.query = search.read(cx).value().trim().to_lowercase();
                    this.schedule_search(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.submit_search(window, cx),
                // leaving the field empty (Esc, a click elsewhere) ends the search and its chat scope
                InputEvent::Blur if this.query.is_empty() => this.end_search(window, cx),
                _ => {}
            },
        )
        .detach();
        let timeline = cx.new(|cx| MessageScrollerState::new(0, cx));
        // scrolling down to the newest message reads it, and drops a deep jump's older rows;
        // scrolling up loads older ones
        cx.observe(&timeline, |this, _, cx| {
            this.mark_read(false, cx);
            this.back_to_newest(cx);
            this.timeline_moved(cx);
        })
        .detach();

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
            depth: PAGE,
            messages_loading: None,
            messages_again: false,
            names: HashMap::new(),
            asked: HashSet::new(),
            pictures: HashMap::new(),
            drafts: Default::default(),
            marking_read: None,
            window_active: window.is_window_active(),
            window_visible: window.is_visible(),
            composer,
            composer_empty: true,
            tray: vec![],
            adding: 0,
            send_waiting: false,
            search,
            query: String::new(),
            find: Default::default(),
            timeline,
            contacts: vec![],
            dialog: None,
            answering: false,
            chat_actions: Default::default(),
            actions: actions::Actions::new(window, cx),
            files: Default::default(),
            paging: Default::default(),
            compose: Default::default(),
            previews: Default::default(),
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
            crate::Demo::Chat
            | crate::Demo::NewGroup
            | crate::Demo::Reply
            | crate::Demo::Compose
            | crate::Demo::Search
            | crate::Demo::SearchInChat
            | crate::Demo::History
            | crate::Demo::LinkPreview => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.contacts = addable(&app.chats, app.own_aci.as_deref());
                app.selected = Some("g:01".into());
                app.set_messages(crate::demo::messages(), cx);
                app.demo_actions(matches!(screen, crate::Demo::Reply));
                let note = format!("c:{}", crate::demo::ME);
                app.drafts.keep(&note, "Renew the passport\nbefore June".into());
                if let crate::Demo::NewGroup = screen {
                    let ana = app.contacts.first().map(|(aci, _)| aci.clone());
                    app.open_dialog(DialogKind::Group(ana.into_iter().collect()), window, cx);
                }
                if let crate::Demo::Compose = screen {
                    app.demo_compose(window, cx);
                }
                if let crate::Demo::Search | crate::Demo::SearchInChat = screen {
                    app.demo_search(matches!(screen, crate::Demo::SearchInChat), window, cx);
                }
                if let crate::Demo::History = screen {
                    app.demo_paging(cx);
                }
                if let crate::Demo::LinkPreview = screen {
                    app.demo_previews(window, cx);
                }
            }
            crate::Demo::ChatMenu | crate::Demo::Request | crate::Demo::Archive => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.demo_chats(screen, cx);
            }
            crate::Demo::Files => {
                app.screen = Screen::Chats;
                app.chats = crate::demo::chats();
                app.selected = Some(format!("c:{}", crate::demo::BOGDAN));
                let messages = crate::demo::file_messages();
                app.files = files::Files::demo(crate::demo::files(&messages));
                app.set_messages(messages, cx);
                app.demo_tray();
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
        // before the chats: one is opened only once the drafts are in (one database connection)
        let drafts = self.load_drafts(client.clone(), cx);
        cx.set_global(Core(client.clone(), drafts));
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
        match &event {
            CoreEvent::MessageExpired { key, .. } => {
                self.message_expired(key, cx);
                self.forget_hit(key);
            }
            CoreEvent::MessageChanged { key, .. } => self.recheck_hit(key, cx),
            _ => {}
        }
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
                    // its reactions, files or preview may have changed with it
                    self.read_details([(key.author, key.sent_at)], cx);
                    self.reload_messages(cx);
                }
            }
            CoreEvent::AttachmentChanged {
                conversation_id,
                message,
                ..
            } => self.attachment_changed(&conversation_id, &message, cx),
            CoreEvent::Error { message } => self.error = Some(message),
            // not in this version: typing, calls, stories, notifications (a reaction shows with its
            // message's `MessageChanged`)
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
        // an archive that emptied (the last chat unarchived, or written in) closes
        self.chat_actions.archive &= self.chats.iter().any(|c| c.archived);
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
            // from a dialog, whose field had the focus
            self.composer.update(cx, |input, cx| input.focus(window, cx));
            return cx.notify();
        }
        self.switch(Some(id), window, cx);
    }

    /// Leaves the open chat, its unsent text kept as its draft, for `id` (into its composer) or for
    /// none.
    fn switch(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let draft = id
            .as_ref()
            .and_then(|id| self.drafts.get(id))
            .cloned()
            .unwrap_or_default();
        let unsent = self.actions.leave(self.typed(cx)).words();
        if let Some(left) = std::mem::replace(&mut self.selected, id) {
            self.drafts.keep(&left, unsent);
        }
        self.set_typed(compose::Typed::plain(draft), window, cx);
        match self.selected {
            Some(_) => self.composer.update(cx, |input, cx| input.focus(window, cx)),
            None => window.blur(cx),
        }
        self.load_members(cx);
        self.generation += 1;
        self.depth = PAGE;
        self.messages.clear();
        self.clear_files(cx);
        self.clear_previews(cx);
        // the draft's link, as if typed: putting text in the field says nothing
        self.composer_changed(cx);
        self.clear_tray(cx);
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
        let (generation, mut depth) = (self.generation, self.depth);
        if self.messages_loading == Some(generation) {
            self.messages_again = true;
            return;
        }
        self.messages_loading = Some(generation);
        self.messages_again = false;
        cx.spawn(async move |this, cx| loop {
            let page = Compat::new(client.messages(id.clone(), None, depth)).await;
            let again = this.update(cx, |app, cx| {
                if app.generation != generation {
                    return None; // another chat was opened meanwhile
                }
                // more is wanted now (back to a found message): not shown, read again
                if depth < app.depth {
                    app.messages_again = false;
                    return Some(app.depth);
                }
                app.older_loaded();
                match page {
                    Ok(page) => app.set_messages(page, cx),
                    Err(e) => {
                        app.messages_failed();
                        app.error = Some(format!("Couldn't load the messages: {e}"));
                    }
                }
                let again = std::mem::take(&mut app.messages_again);
                if !again {
                    app.messages_loading = None;
                }
                cx.notify();
                again.then_some(app.depth)
            });
            match again {
                Ok(Some(wanted)) => depth = wanted,
                _ => break,
            }
        })
        .detach();
    }

    fn set_messages(&mut self, page: Vec<Message>, cx: &mut Context<Self>) {
        let key = |m: &Message| (m.author.clone(), m.sent_at);
        let old: Vec<_> = self.messages.iter().map(key).collect();
        let new: Vec<_> = page.iter().map(key).collect();
        self.messages = page;
        let changes = changed_rows(&old, &new);
        let came = changes
            .iter()
            .flat_map(|(range, count)| &new[range.start..range.start + count]);
        let came: Vec<_> = came.cloned().collect();
        // only rows that came or went are replaced, so a reader scrolled up stays put; with the
        // same messages, some changed (a tick, an edit) and their heights may have too
        self.timeline.update(cx, |t, cx| {
            if changes.is_empty() {
                t.remeasure(cx);
            }
            for (range, count) in changes {
                t.splice(range, count, cx);
            }
        });
        self.new_rows(&old, &new, cx);
        let mut wanted: Vec<String> = self
            .messages
            .iter()
            .filter(|m| !m.is_outgoing)
            .map(|m| m.author.clone())
            .collect();
        wanted.extend(self.messages.iter().flat_map(|m| mentioned(&m.ranges)));
        wanted.extend(
            self.messages
                .iter()
                .flat_map(|m| m.quote.iter().flat_map(|q| mentioned(&q.ranges))),
        );
        self.load_names(wanted, cx);
        self.messages_loaded(came, cx);
        self.reveal_found(cx);
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
        let Some(id) = self.selected.clone() else { return };
        let typed = self.typed(cx);
        let (body, ranges) = compose::message(&typed);
        let plan = attach::plan(&body, self.actions.editing(), self.adding > 0, !self.tray.is_empty());
        match plan {
            attach::Plan::Nothing => return,
            attach::Plan::Edit => {
                self.save_edit(body, ranges, window, cx);
                return;
            }
            attach::Plan::Wait => return self.send_when_added(),
            attach::Plan::Text | attach::Plan::Files { .. } => {}
        }
        let Some(client) = self.client.clone() else { return };
        let reply = self.take_reply();
        let quote = reply.as_ref().map(actions::quote);
        let preview = self.previews.take(&body);
        self.set_typed(compose::Typed::default(), window, cx);
        let files = self.take_tray();
        let worded = !body.is_empty();
        cx.spawn_in(window, async move |this, cx| {
            let sent = match plan {
                // a preview goes with text only (`AppStore.send`)
                attach::Plan::Files { caption } => {
                    let outgoing = files.iter().map(attach::Outgoing::file).collect();
                    Compat::new(client.send_attachments(id.clone(), outgoing, caption, ranges, quote)).await
                }
                _ => Compat::new(client.send_text(id.clone(), body, ranges, quote, preview.clone())).await,
            };
            let had_files = !files.is_empty();
            let _ = this.update(cx, |app, cx| app.tray_back(&id, files, sent.is_ok(), cx));
            let Err(e) = sent else { return };
            let _ = this.update_in(cx, |app, window, cx| {
                // the Mac app's words: the core's
                let e = describe(&e);
                // the text goes back to its chat: into the composer if that chat is still open
                // and nothing new was typed, else into its draft if it has none (in an edit, the
                // one from before it); a draft there stays, and the words are lost (as on the Mac)
                let open = app.selected.as_deref() == Some(id.as_str());
                let editing = open && app.actions.editing();
                if open && !editing && app.composer.read(cx).value().trim().is_empty() {
                    app.error = Some(e);
                    app.set_typed(typed, window, cx);
                    app.restore_reply(reply);
                    app.previews.put_back(preview);
                } else {
                    // the files went back to the tray only if their chat is still open
                    let lost_files = had_files && !open;
                    let back = worded && app.drafts.get(&id).is_none();
                    app.error = Some(attach::unsent(&e, back, reply.is_some(), lost_files));
                    if back {
                        app.drafts.keep(&id, typed.words());
                        if editing {
                            app.actions.set_aside(typed);
                        }
                    }
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
                let id = preview.conversation_id.clone();
                match join_step(preview, listed(&id)) {
                    JoinStep::Open => {
                        self.dialog = None;
                        return self.select(id, window, cx);
                    }
                    JoinStep::List => async move {
                        match client.list_group(value).await {
                            Ok(()) => Outcome::Open(id),
                            Err(e) => Outcome::Problem(format!("Couldn't open the group: {}", describe(&e))),
                        }
                    }
                    .boxed_local(),
                    JoinStep::Join | JoinStep::AskToJoin => async move {
                        match client.join_group(value).await {
                            Ok(joined) => Outcome::Open(joined.conversation_id),
                            Err(e) => Outcome::Problem(server_failure("Couldn't join the group", &e)),
                        }
                    }
                    .boxed_local(),
                }
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
        if self.answering {
            return;
        }
        self.answering = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let cancelled = Compat::new(client.cancel_join_request(id.clone())).await;
            let _ = this.update(cx, |app, cx| {
                app.answering = false;
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
            self.clear_previews(cx);
            self.clear_tray(cx);
            self.timeline.update(cx, |t, cx| t.reset(0, cx));
        }
        self.drop_chat_copies(&id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let deleted = Compat::new(client.delete_chat(id.clone())).await;
            let _ = this.update(cx, |app, cx| {
                match deleted {
                    // the core deleted its draft too: cleared after the writes on their way, so
                    // none brings it back
                    Ok(()) => app.drafts.keep(&id, String::new()),
                    Err(e) => app.error = Some(format!("Couldn't delete the chat: {}", describe(&e))),
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
    // asked from another device, with no chat here: asking again lists it
    if listed && (p.member || p.requested) {
        JoinStep::Open
    } else if p.member {
        // the server refuses to add a member again
        JoinStep::List
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

/// The rows that changed between two lists of keys, in order: each `(range, count)` replaces the
/// rows in `range` of the list as the ones before it left it with `count` rows of `new`. The rows
/// both have stay (messages keep their order), so a scroll position on one of them holds.
fn changed_rows<K: Eq + Hash>(old: &[K], new: &[K]) -> Vec<(Range<usize>, usize)> {
    let (in_old, in_new): (HashSet<&K>, HashSet<&K>) = (old.iter().collect(), new.iter().collect());
    let (mut i, mut j, mut changes) = (0, 0, vec![]);
    while i < old.len() || j < new.len() {
        if old.get(i).is_some_and(|k| new.get(j) == Some(k)) {
            (i, j) = (i + 1, j + 1);
            continue;
        }
        let (from_i, from_j) = (i, j);
        while old.get(i).is_some_and(|k| !in_new.contains(k)) {
            i += 1;
        }
        while new.get(j).is_some_and(|k| !in_old.contains(k)) {
            j += 1;
        }
        // rows both have, in another order: the rest is replaced
        if (i, j) == (from_i, from_j) {
            (i, j) = (old.len(), new.len());
        }
        // `new[..from_j]` is in place by now, and `old[from_i..]` follows it
        changes.push((from_j..from_j + i - from_i, j - from_j));
    }
    changes
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

    pub(super) fn chat(id: &str, title: &str) -> Conversation {
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
        // in it, but its chat isn't listed here (no messages yet): Open Chat lists it first (the
        // server refuses to add a member again)
        assert_eq!(join_step(&member, false), JoinStep::List);
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
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 2, 3, 4]), [(3..3, 1)]);
        // the window moved on by one: the oldest went, a new one came
        assert_eq!(changed_rows(&[1, 2, 3], &[2, 3, 4]), [(0..1, 0), (2..2, 1)]);
        // an older page came above
        assert_eq!(changed_rows(&[3, 4], &[1, 2, 3, 4]), [(0..0, 2)]);
        // back to the newest: the older ones went
        assert_eq!(changed_rows(&[1, 2, 3, 4], &[3, 4]), [(0..2, 0)]);
        // one expired in the middle
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 3]), [(1..2, 0)]);
        // one came and one went in the same place
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 5, 3]), [(1..2, 1)]);
        // nothing came or went
        assert!(changed_rows(&[1, 2], &[1, 2]).is_empty());
        // the first load, and a chat that emptied
        assert_eq!(changed_rows(&[], &[1, 2]), [(0..0, 2)]);
        assert_eq!(changed_rows(&[1, 2], &[]), [(0..2, 0)]);
        // rows in another order: from there on, all are replaced
        assert_eq!(changed_rows(&[1, 2, 3], &[1, 3, 2]), [(1..3, 2)]);
    }

    #[test]
    fn the_changed_rows_turn_the_old_list_into_the_new() {
        let cases: [(&[u32], &[u32]); 5] = [
            (&[1, 2, 3, 4, 5], &[0, 2, 4, 6]),
            (&[1, 2, 3], &[7, 8, 1, 9, 3, 10]),
            (&[5, 6], &[1, 2, 3]),
            (&[1, 2, 3, 4], &[4, 3, 2, 1]),
            (&[2, 4, 6, 8], &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ];
        for (old, new) in cases {
            let mut rows = old.to_vec();
            for (range, count) in changed_rows(old, new) {
                // the rows that come are new's, after those already in place
                let start = range.start;
                rows.splice(range, new[start..start + count].iter().copied());
            }
            assert_eq!(rows, new, "{old:?} to {new:?}");
        }
    }
}
