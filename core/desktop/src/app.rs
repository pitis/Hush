//! The window: linking, the chat list, a chat's messages and the composer. A subset of the Mac
//! app's rules (`AppStore.swift`); where this one is simpler, a `shortcut:` comment says so.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_compat::Compat;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::bubble::{Bubble, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message::MessageAlignment;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Sizable, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind, Message, MessageKey, TextStyle};
use sig_core::{Client, ConnectionState, CoreEvent, EventListener, HistoryStage};

use crate::{key, qr, text};

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

/// A message as the timeline shows it.
struct Row {
    outgoing: bool,
    /// The sender's name, above the first of their messages in a run (groups only).
    author: Option<String>,
    text: String,
    footer: String,
    /// What assistive technology reads: who, what, when.
    label: String,
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
    timeline: Entity<MessageScrollerState>,
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
        cx.subscribe_in(&composer, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(window, cx);
            }
        })
        .detach();
        let timeline = cx.new(|cx| MessageScrollerState::new(0, cx));
        // scrolling down to the newest message reads it
        cx.observe(&timeline, |this, _, cx| this.mark_read(false, cx)).detach();

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
            drafts: HashMap::new(),
            marking_read: None,
            window_active: window.is_window_active(),
            window_visible: window.is_visible(),
            composer,
            timeline,
        }
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
        self.mark_read(false, cx);
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

    fn rows(&self, group: bool) -> Vec<Row> {
        let mut previous: Option<&str> = None;
        let rows = self.messages.iter().map(|m| {
            let first_of_run = previous != Some(m.author.as_str());
            previous = Some(m.author.as_str());
            let time = text::time(m.sent_at);
            let text = text::message(m, |aci| self.name(aci));
            let footer = if m.is_outgoing {
                format!("{time} · {}", text::status(m.status))
            } else {
                time
            };
            Row {
                outgoing: m.is_outgoing,
                author: (group && !m.is_outgoing && first_of_run).then(|| self.name(&m.author)),
                label: format!("{}: {text}. {footer}", self.name(&m.author)),
                text,
                footer,
            }
        });
        rows.collect()
    }

    fn render_link(&self, url: Option<&str>, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let code = match (url, &self.error) {
            (Some(url), _) => qr::code(url, 280.).map(IntoElement::into_any_element),
            (None, Some(_)) => None,
            (None, None) => Some(Spinner::new().into_any_element()),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .p_10()
            .child(
                label("link-title", Role::Heading, "Link Hush to your phone")
                    .aria_level(1)
                    .text_xl()
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                label(
                    "link-how",
                    Role::Label,
                    "On your phone: Settings → Linked devices → Link new device, then scan this code.",
                )
                .max_w(px(360.))
                .text_center()
                .text_color(muted),
            )
            .child(
                label(
                    "link-history",
                    Role::Label,
                    "If your phone offers to transfer your message history, accept it to see your older messages here.",
                )
                .max_w(px(360.))
                .text_center()
                .text_sm()
                .text_color(muted),
            )
            .child(
                div()
                    .id("link-code")
                    .when(url.is_some(), |this| {
                        this.role(Role::Image).aria_label("Linking QR code")
                    })
                    .size(px(280.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(code),
            )
            .when_some(self.error.clone().filter(|_| url.is_none()), |this, error| {
                this.child(
                    label("link-error", Role::Alert, error)
                        .max_w(px(360.))
                        .text_center()
                        .text_sm()
                        .text_color(danger),
                )
                .child(
                    Button::new("retry")
                        .primary()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_link(cx))),
                )
            })
    }

    fn render_chat_rows(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let theme = cx.theme();
        let (muted, active, hover, primary, on_primary) = (
            theme.muted_foreground,
            theme.list_active,
            theme.list_hover,
            theme.primary,
            theme.primary_foreground,
        );
        range
            .map(|ix| {
                let c = &self.chats[ix];
                let title = text::title(c, self.own_aci.as_deref());
                let line = text::preview(c, &self.name(&c.last_message_author), |aci| self.name(aci));
                let unread = (c.unread_count > 0 || c.marked_unread).then(|| match c.unread_count {
                    0 => String::new(),
                    n => n.to_string(),
                });
                // a blocked chat's or a request's picture isn't shown (a stranger chose it)
                let picture = c.avatar.clone().filter(|_| !c.blocked && !c.message_request);
                let spoken = match &unread {
                    Some(count) if !count.is_empty() => format!("{title}, {count} unread. {line}"),
                    Some(_) => format!("{title}, unread. {line}"),
                    None => format!("{title}. {line}"),
                };
                let selected = self.selected.as_deref() == Some(c.id.as_str());
                let id = c.id.clone();
                h_flex()
                    .id(ix)
                    .role(Role::ListItem)
                    .aria_label(spoken)
                    .aria_selected(selected)
                    .h(px(64.))
                    .px_3()
                    .gap_3()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(active))
                    .hover(|style| style.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.select(id.clone(), window, cx)))
                    .child(
                        Avatar::new()
                            .name(title.clone())
                            .when_some(picture, |a, p| a.src(PathBuf::from(p))),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(div().truncate().font_weight(FontWeight::MEDIUM).child(title))
                                    .when(c.last_message_at > 0, |this| {
                                        this.child(
                                            div()
                                                .flex_none()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(text::time(c.last_message_at)),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().truncate().text_sm().text_color(muted).child(line))
                                    .when_some(unread, |this, count| {
                                        this.child(
                                            div()
                                                .flex_none()
                                                .min_w(px(18.))
                                                .h(px(18.))
                                                .px_1()
                                                .rounded_full()
                                                .bg(primary)
                                                .text_color(on_primary)
                                                .text_xs()
                                                .text_center()
                                                .child(count),
                                        )
                                    }),
                            ),
                    )
            })
            .collect()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let status = match self.connection {
            ConnectionState::Connecting => Some("Connecting…"),
            ConnectionState::Offline => Some("Offline"),
            ConnectionState::Online | ConnectionState::Unlinked => None,
        };
        let offline = self.connection == ConnectionState::Offline;
        let history = self.history.map(|h| self.render_history(h, cx));
        let reconnect = offline.then(|| {
            Button::new("reconnect")
                .xsmall()
                .label("Reconnect")
                .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx)))
        });
        let theme = cx.theme();
        v_flex()
            .w(px(320.))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .h(px(48.))
                    .px_4()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .child(
                        label("chats-title", Role::Heading, "Chats")
                            .aria_level(1)
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .children(status.map(|s| {
                                label("connection", Role::Status, s)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                            }))
                            .children(reconnect),
                    ),
            )
            .children(history)
            .child(
                div()
                    .id("chat-list")
                    .role(Role::List)
                    .aria_label("Chats")
                    .flex_1()
                    .min_h_0()
                    .child(uniform_list("chats", self.chats.len(), cx.processor(Self::render_chat_rows)).size_full()),
            )
    }

    /// The history transfer's progress, with a way out at every stage (the Mac app's
    /// `HistoryView.swift` words).
    fn render_history(&self, (stage, done, total): (HistoryStage, u64, u64), cx: &mut Context<Self>) -> Div {
        let failed = stage == HistoryStage::Failed;
        let title = match stage {
            HistoryStage::Waiting => "Preparing to transfer…",
            HistoryStage::Downloading => "Transferring your messages…",
            HistoryStage::Importing => "Importing your messages…",
            HistoryStage::Failed => "The transfer didn't finish",
            HistoryStage::Done | HistoryStage::NoHistory => "",
        };
        let detail = match stage {
            HistoryStage::Waiting => Some("Keep the app open on your phone.".to_string()),
            HistoryStage::Downloading if total > 0 => Some(format!(
                "{:.0} of {:.0} MB",
                done as f64 / 1_048_576.,
                total as f64 / 1_048_576.
            )),
            HistoryStage::Importing if done > 0 => Some(format!("{done} items")),
            HistoryStage::Failed => Some("New messages arrive once you continue.".to_string()),
            _ => None,
        };
        let theme = cx.theme();
        v_flex()
            .m_3()
            .p_3()
            .gap_2()
            .flex_none()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                label("history-title", Role::Status, title)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM),
            )
            .children(detail.map(|d| {
                label("history-detail", Role::Label, d)
                    .text_xs()
                    .text_color(theme.muted_foreground)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("history-continue")
                            .small()
                            .label("Continue without history")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_history(cx))),
                    )
                    .when(failed, |this| {
                        this.child(
                            Button::new("history-retry")
                                .small()
                                .primary()
                                .label("Try again")
                                .on_click(cx.listener(|this, _, _, cx| this.retry_history(cx))),
                        )
                    }),
            )
    }

    fn render_chat(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let error = self.error.clone().map(|e| {
            label("error", Role::Alert, e)
                .px_4()
                .py_2()
                .flex_none()
                .text_sm()
                .text_color(danger)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.error = None;
                    cx.notify();
                }))
        });
        let open = self
            .selected
            .as_ref()
            .and_then(|id| self.chats.iter().find(|c| &c.id == id));
        let Some(c) = open else {
            return v_flex().flex_1().h_full().children(error).child(
                div().flex_1().flex().items_center().justify_center().child(
                    label(
                        "no-chat",
                        Role::Label,
                        if self.chats.is_empty() {
                            "No chats yet"
                        } else {
                            "Choose a chat"
                        },
                    )
                    .text_color(muted),
                ),
            );
        };
        let rows = Rc::new(self.rows(c.kind == ConversationKind::Group));
        // shortcut: requests, invitations, join requests and blocked chats are answered on the
        // phone; the Mac app offers Accept, Block and the others here
        let answer_first = c.message_request || c.group_invitation || c.group_join_requested || c.blocked;
        let answer_where = if c.blocked {
            "This chat is blocked. Unblock it on your phone to write here."
        } else {
            "Answer this chat on your phone to write here."
        };
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                h_flex()
                    .h(px(48.))
                    .px_4()
                    .flex_none()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        label("chat-title", Role::Heading, text::title(c, self.own_aci.as_deref()))
                            .aria_level(2)
                            .font_weight(FontWeight::SEMIBOLD),
                    ),
            )
            .children(error)
            .child(
                MessageScroller::new("timeline", self.timeline.clone(), move |ix, _, cx| {
                    render_row(ix, &rows[ix], cx)
                })
                .flex_1()
                .min_h_0(),
            )
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .when(answer_first, |this| {
                        this.child(
                            label("answer-first", Role::Label, answer_where)
                                .text_sm()
                                .text_color(muted),
                        )
                    })
                    .when(!answer_first, |this| {
                        this.child(Textarea::new(&self.composer).aria_label("Message"))
                    }),
            )
    }

    fn render_notice(&self, title: &str, detail: String, cx: &mut Context<Self>) -> Div {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_10()
            .child(
                label("notice-title", Role::Heading, title.to_owned())
                    .aria_level(1)
                    .text_xl()
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                label("notice-detail", Role::Label, detail)
                    .max_w(px(420.))
                    .text_center()
                    .text_color(cx.theme().muted_foreground),
            )
    }
}

fn render_row(ix: usize, row: &Row, cx: &mut App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    let (alignment, variant) = match row.outgoing {
        true => (MessageAlignment::End, BubbleVariant::Filled),
        false => (MessageAlignment::Start, BubbleVariant::Secondary),
    };
    v_flex()
        .id(ix)
        .role(Role::ListItem)
        .aria_label(row.label.clone())
        .w_full()
        .px_4()
        .py_1()
        .gap_1()
        .when(row.outgoing, |this| this.items_end())
        .children(row.author.clone().map(|a| div().text_xs().text_color(muted).child(a)))
        .child(
            Bubble::new()
                .alignment(alignment)
                .with_variant(variant)
                .child(row.text.clone()),
        )
        .child(div().text_xs().text_color(muted).child(row.footer.clone()))
}

/// Text that assistive technology reads too: GPUI shows it only elements with an id and a role;
/// the label is what it reads.
fn label(id: impl Into<ElementId>, role: Role, text: impl Into<SharedString>) -> Stateful<Div> {
    let text = text.into();
    div().id(id).role(role).aria_label(text.clone()).child(text)
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

impl Render for Hush {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground) = (cx.theme().background, cx.theme().foreground);
        let screen = match &self.screen {
            Screen::Opening => div().size_full().flex().items_center().justify_center().child(
                div()
                    .id("opening")
                    .role(Role::ProgressIndicator)
                    .aria_label("Opening")
                    .child(Spinner::new()),
            ),
            Screen::Link { url } => self.render_link(url.clone().as_deref(), cx),
            Screen::Chats => h_flex()
                .size_full()
                .child(self.render_sidebar(cx))
                .child(self.render_chat(cx)),
            Screen::Unlinked => {
                let detail = format!(
                    "Linking it again isn't in this version yet: quit Hush, delete the folder {}, and open Hush again.",
                    self.data_dir.display()
                );
                self.render_notice("This computer was unlinked from your phone", detail, cx)
            }
            Screen::Failed(e) => self.render_notice("Hush couldn't start", e.clone(), cx),
        };
        div().size_full().bg(background).text_color(foreground).child(screen)
    }
}

#[cfg(test)]
mod tests {
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
