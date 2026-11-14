//! What can be done to a message (the Mac app's `MessageActions.swift`, after Signal Desktop):
//! react, reply, edit, copy, retry and delete for everyone; its reactions, and the reply or edit
//! waiting in the composer.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{Message, MessageKey, MessageStatus, Quote, Reaction, TextStyle};

use super::Hush;
use crate::text;

pub(super) mod view;

/// Signal's default reactions.
pub const QUICK_REACTIONS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];
/// Signal lets you edit or delete a message for everyone this long after sending it.
const CHANGE_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// A message's identity as a map key (`MessageKey` isn't `Hash`).
type Key = (String, i64);

fn key(m: &Message) -> Key {
    (m.author.clone(), m.sent_at)
}

/// Our own message, sent, not deleted, within the window (the core checks this again).
pub fn can_delete(m: &Message, me: Option<&str>, now_ms: i64) -> bool {
    let sent = matches!(
        m.status,
        MessageStatus::Sent | MessageStatus::Delivered | MessageStatus::Read
    );
    me.is_some_and(|me| m.author == me) && m.is_outgoing && !m.deleted && sent && now_ms - m.sent_at <= CHANGE_WINDOW_MS
}

/// As `can_delete`, and it has text to edit. Not a reply to a story: an edit goes without the
/// story it answers.
/// shortcut: this composer holds no styles or mentions, so a message with any isn't offered (an
/// edit would lose them); the Mac app's composer holds both and lets those through.
pub fn can_edit(m: &Message, me: Option<&str>, now_ms: i64) -> bool {
    can_delete(m, me, now_ms)
        && m.body.as_deref().is_some_and(|b| !b.is_empty())
        && m.story.is_none()
        && m.ranges.is_empty()
}

/// Our reaction among a message's reactions.
pub fn mine<'a>(reactions: &'a [Reaction], me: Option<&str>) -> Option<&'a str> {
    reactions
        .iter()
        .find(|r| Some(r.reactor.as_str()) == me)
        .map(|r| r.emoji.as_str())
}

/// Whether choosing `emoji` takes our reaction back: our current one again does, any other
/// replaces it (one reaction each).
pub fn removes(emoji: &str, mine: Option<&str>) -> bool {
    Some(emoji) == mine
}

#[derive(Clone, Debug, PartialEq)]
pub struct PillEntry {
    pub emoji: String,
    pub count: usize,
    pub mine: bool,
}

/// The reaction pill: each emoji once with how many, most first, ours marked.
pub fn pill(reactions: &[Reaction], me: Option<&str>) -> Vec<PillEntry> {
    let ours = mine(reactions, me);
    let mut entries: Vec<PillEntry> = vec![];
    for r in reactions {
        match entries.iter_mut().find(|e| e.emoji == r.emoji) {
            Some(e) => e.count += 1,
            None => entries.push(PillEntry {
                emoji: r.emoji.clone(),
                count: 1,
                mine: Some(r.emoji.as_str()) == ours,
            }),
        }
    }
    // stable: equal counts keep the order they first came in
    entries.sort_by_key(|e| std::cmp::Reverse(e.count));
    entries
}

/// ↑ in an empty composer: the newest message of ours that can still be edited.
pub fn last_editable<'a>(messages: &'a [Message], me: Option<&str>, now_ms: i64) -> Option<&'a Message> {
    messages.iter().rev().find(|m| can_edit(m, me, now_ms))
}

/// A message's menu item (its right-click menu and its … button).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item {
    React,
    Reply,
    Edit,
    Copy,
    Retry,
    Delete,
}

/// The items a message's menu offers, in order (`MessageRow.menuItems`).
pub fn menu(m: &Message, me: Option<&str>, now_ms: i64) -> Vec<Item> {
    let mut items = vec![];
    if !m.deleted {
        items.extend([Item::React, Item::Reply]);
    }
    if can_edit(m, me, now_ms) {
        items.push(Item::Edit);
    }
    if m.body.is_some() && !m.deleted {
        items.push(Item::Copy);
    }
    if m.status == MessageStatus::Failed {
        items.push(Item::Retry);
    }
    if can_delete(m, me, now_ms) {
        items.push(Item::Delete);
    }
    items
}

/// What a reply sends along: the message it answers.
pub(super) fn quote(m: &Message) -> Quote {
    Quote {
        author: m.author.clone(),
        sent_at: m.sent_at,
        text: m.body.clone(),
        ranges: m.ranges.clone(),
    }
}

/// The open chat's reactions, and the reply or edit in the composer.
pub(super) struct Actions {
    /// By message.
    reactions: HashMap<Key, Vec<Reaction>>,
    /// Our reactions sent but not loaded back yet (None: taken back), with the load they came
    /// after, so a second click acts on what we just did. A later load is the core's word.
    pending: HashMap<Key, (Option<String>, u64)>,
    /// Bumped by each reactions load and each chat change: only the newest load is kept.
    loads: u64,
    reply_to: Option<Message>,
    /// Our message being edited in the composer; it ends replying (the two don't mix).
    editing: Option<Message>,
    /// What was typed before editing started; back once the edit is saved or cancelled.
    draft_before_edit: Option<String>,
    /// The message whose reaction picker is open: its buttons stay shown.
    picking: Option<Key>,
    /// Takes the keyboard when message text is selected: in the composer, ⌘C would copy
    /// the composer's own (empty) selection.
    focus: FocusHandle,
    window: AnyWindowHandle,
}

impl Actions {
    pub(super) fn new(window: &Window, cx: &mut App) -> Self {
        Self {
            reactions: HashMap::new(),
            pending: HashMap::new(),
            loads: 0,
            reply_to: None,
            editing: None,
            draft_before_edit: None,
            picking: None,
            focus: cx.focus_handle(),
            window: window.window_handle(),
        }
    }

    /// Another chat opens: nothing follows it there. Returns the draft of the one left (while
    /// editing, what was typed before).
    pub(super) fn leave(&mut self, typed: String) -> String {
        let draft = match self.editing.take() {
            Some(_) => self.draft_before_edit.take().unwrap_or_default(),
            None => typed,
        };
        self.reactions.clear();
        self.pending.clear();
        self.loads += 1;
        self.reply_to = None;
        self.picking = None;
        draft
    }

    pub(super) fn focus(&self) -> &FocusHandle {
        &self.focus
    }

    pub(super) fn editing(&self) -> bool {
        self.editing.is_some()
    }
}

/// What a row shows of a message's actions and reactions.
pub(super) struct RowActions {
    pub message: Message,
    pub items: Vec<Item>,
    /// Ours, counting one just sent.
    pub mine: Option<String>,
    /// The message it replies to, above its text: a header, the text and its "@name"s.
    pub quote: Option<(&'static str, String, Vec<Range<usize>>)>,
    pub pill: Vec<PillEntry>,
    /// Who reacted: emoji, name, ours.
    pub reactors: Vec<(String, String, bool)>,
    /// The text Copy copies: mentions as "@name", spoilers shown.
    pub copy: Option<String>,
    pub picking: bool,
}

impl Hush {
    pub(super) fn row_actions(&self, m: &Message, now_ms: i64) -> Rc<RowActions> {
        let me = self.own_aci.as_deref();
        let k = key(m);
        let reactions = self.actions.reactions.get(&k).map(Vec::as_slice).unwrap_or_default();
        let name = |aci: &str| self.name(aci);
        let quote = m.quote.as_ref().filter(|_| !m.deleted).map(|q| {
            let header = if q.author == m.author { "Reply" } else { "Quoted" };
            let (text, mentions) = match &q.text {
                Some(t) => text::marked(t, &q.ranges, name),
                None => ("Attachment".into(), vec![]),
            };
            (header, text, mentions)
        });
        let shown: Vec<_> = m
            .ranges
            .iter()
            .filter(|r| r.style != TextStyle::Spoiler)
            .cloned()
            .collect();
        Rc::new(RowActions {
            message: m.clone(),
            items: menu(m, me, now_ms),
            mine: self.my_reaction(m),
            quote,
            pill: pill(reactions, me),
            reactors: reactions
                .iter()
                .map(|r| (r.emoji.clone(), self.name(&r.reactor), Some(r.reactor.as_str()) == me))
                .collect(),
            copy: m
                .body
                .as_deref()
                .filter(|_| !m.deleted)
                .map(|b| text::plain(b, &shown, name)),
            picking: self.actions.picking.as_ref() == Some(&k),
        })
    }

    /// Our reaction to a message, counting one just sent.
    fn my_reaction(&self, m: &Message) -> Option<String> {
        let k = key(m);
        match self.actions.pending.get(&k) {
            Some((pending, _)) => pending.clone(),
            None => {
                let reactions = self.actions.reactions.get(&k).map(Vec::as_slice).unwrap_or_default();
                mine(reactions, self.own_aci.as_deref()).map(str::to_owned)
            }
        }
    }

    /// The open chat's messages were loaded again: their reactions are read, and an edit of one
    /// that was deleted meanwhile (here or on the phone) ends.
    pub(super) fn messages_loaded(&mut self, cx: &mut Context<Self>) {
        let gone = self.actions.editing.as_ref().is_some_and(|e| {
            let k = key(e);
            self.messages.iter().any(|m| key(m) == k && m.deleted)
        });
        if gone {
            // restoring the draft needs the window, which a load doesn't have
            let (this, window) = (cx.entity().downgrade(), self.actions.window);
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    this.update(cx, |app, cx| app.stop_editing(window, cx))
                });
            });
        }
        let Some(client) = self.client.clone() else { return };
        self.actions.loads += 1;
        let load = self.actions.loads;
        let keys: Vec<MessageKey> = self.messages.iter().map(Message::key).collect();
        cx.spawn(async move |this, cx| {
            let details = Compat::new(client.details(keys)).await;
            let _ = this.update(cx, |app, cx| {
                if app.actions.loads != load {
                    return; // a newer load, or another chat
                }
                let before = app.actions.reactions.clone();
                // a message the core couldn't read isn't in it, and keeps what it showed
                for d in details {
                    let k = (d.key.author, d.key.sent_at);
                    if d.reactions.is_empty() {
                        app.actions.reactions.remove(&k);
                    } else {
                        app.actions.reactions.insert(k, d.reactions);
                    }
                }
                app.actions.pending.retain(|_, (_, after)| *after >= load);
                if app.actions.reactions != before {
                    app.timeline.update(cx, |t, cx| t.remeasure(cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reacts to a message: our current reaction again takes it back, another replaces it.
    pub(super) fn react(&mut self, m: &Message, emoji: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let remove = removes(emoji, self.my_reaction(m).as_deref());
        let (k, target, emoji) = (key(m), m.key(), emoji.to_owned());
        self.actions.picking = None;
        cx.spawn(async move |this, cx| {
            let sent = Compat::new(client.react(target, emoji.clone(), remove)).await;
            let _ = this.update(cx, |app, cx| {
                match sent {
                    Ok(()) => {
                        let load = app.actions.loads;
                        app.actions.pending.insert(k, ((!remove).then_some(emoji), load));
                    }
                    Err(e) => app.error = Some(format!("Couldn't react: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Replies to a message (ends editing: the two don't mix).
    pub(super) fn reply(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        if self.actions.editing.is_some() {
            self.stop_editing(window, cx);
        }
        self.actions.reply_to = Some(m.clone());
        self.composer.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Puts one of our messages in the composer to edit it.
    pub(super) fn start_editing(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        self.actions.reply_to = None;
        if self.actions.editing.is_none() {
            self.actions.draft_before_edit = Some(self.composer.read(cx).value().to_string());
        }
        self.actions.editing = Some(m.clone());
        let body = m.body.clone().unwrap_or_default();
        self.composer_empty = body.trim().is_empty();
        self.composer.update(cx, |input, cx| {
            input.set_value(body, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// The edit ends: what was typed before comes back.
    fn stop_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.actions.editing.take().is_none() {
            return;
        }
        let draft = self.actions.draft_before_edit.take().unwrap_or_default();
        self.composer_empty = draft.trim().is_empty();
        self.composer.update(cx, |input, cx| input.set_value(draft, window, cx));
        cx.notify();
    }

    /// Esc: drops the edit or the reply. False if there was neither.
    pub(super) fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.actions.editing.is_some() {
            self.stop_editing(window, cx);
        } else if self.actions.reply_to.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// ↑ in an empty composer: edits our newest message that can still be edited. False if none.
    pub(super) fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.actions.editing.is_some() || !self.composer.read(cx).value().is_empty() {
            return false;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let Some(m) = last_editable(&self.messages, self.own_aci.as_deref(), now).cloned() else {
            return false;
        };
        self.start_editing(&m, window, cx);
        true
    }

    /// The reply the next message goes with, taken out of the composer.
    pub(super) fn take_reply(&mut self) -> Option<Message> {
        self.actions.reply_to.take()
    }

    /// The send failed and its text went back into the composer: so does its reply.
    pub(super) fn restore_reply(&mut self, reply: Option<Message>) {
        if self.actions.editing.is_none() && self.actions.reply_to.is_none() {
            self.actions.reply_to = reply;
        }
    }

    /// Sends `body` as the new text of the message being edited. False if nothing is edited.
    pub(super) fn save_edit(&mut self, body: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(client), Some(m)) = (self.client.clone(), self.actions.editing.clone()) else {
            return false;
        };
        self.composer.update(cx, |input, cx| input.clean(window, cx));
        cx.spawn_in(window, async move |this, cx| {
            let saved = Compat::new(client.edit(m.key(), body.clone(), vec![])).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let still = app.actions.editing.as_ref().is_some_and(|e| key(e) == key(&m));
                match saved {
                    Ok(()) if still => app.stop_editing(window, cx),
                    Ok(()) => {}
                    Err(e) => {
                        app.error = Some(format!("Couldn't edit the message: {e}"));
                        if still && app.composer.read(cx).value().is_empty() {
                            app.composer_empty = false;
                            app.composer.update(cx, |input, cx| input.set_value(body, window, cx));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    /// Sends a failed message again.
    pub(super) fn retry(&mut self, m: &Message, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let target = m.key();
        cx.spawn(async move |this, cx| {
            // false: it isn't failed any more, nothing to do
            if let Err(e) = Compat::new(client.retry(target)).await {
                let _ = this.update(cx, |app, cx| {
                    app.error = Some(format!("Couldn't send the message: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Asks first, as the Mac app's confirmation: everyone sees that it was deleted.
    pub(super) fn confirm_delete(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Delete this message for everyone?",
            Some("Everyone in this chat sees that you deleted it."),
            &["Delete for Everyone", "Cancel"],
            cx,
        );
        let target = m.key();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let client = this.update(cx, |app, _| app.client.clone()).ok().flatten();
            let Some(client) = client else { return };
            if let Err(e) = Compat::new(client.delete_for_everyone(target)).await {
                let _ = this.update(cx, |app, cx| {
                    app.error = Some(format!("Couldn't delete the message: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The reaction picker opened or closed.
    pub(super) fn set_picking(&mut self, m: &Message, open: bool, cx: &mut Context<Self>) {
        let k = key(m);
        if open {
            self.actions.picking = Some(k);
        } else if self.actions.picking.as_ref() == Some(&k) {
            self.actions.picking = None;
        }
        cx.notify();
    }

    /// The reply or edit above the composer: icon, title, what it's about, and what ✕ is called.
    pub(super) fn composer_bar(&self) -> Option<(&'static str, String, String, &'static str)> {
        let name = |aci: &str| self.name(aci);
        if let Some(e) = &self.actions.editing {
            let detail = e
                .body
                .as_deref()
                .map(|b| text::plain(b, &e.ranges, name))
                .unwrap_or_default();
            return Some(("pencil", "Editing message".into(), detail, "Cancel editing"));
        }
        let r = self.actions.reply_to.as_ref()?;
        let title = if r.is_outgoing {
            "Replying to yourself".into()
        } else {
            format!("Replying to {}", self.name(&r.author))
        };
        let detail = match r.body.as_deref() {
            Some(b) => text::plain(b, &r.ranges, name),
            None => text::placeholder(&r.kind).unwrap_or_else(|| "Attachment".into()),
        };
        Some(("reply", title, detail, "Cancel reply"))
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s made-up reactions; `reply`: a reply waits in the composer, the buttons of the
    /// message it answers show.
    pub(super) fn demo_actions(&mut self, reply: bool) {
        for (k, reactions) in crate::demo::reactions(&self.messages) {
            self.actions.reactions.insert((k.author, k.sent_at), reactions);
        }
        let answered = self
            .messages
            .iter()
            .rev()
            .find(|m| !m.is_outgoing && !m.deleted)
            .cloned();
        if let Some(m) = answered.filter(|_| reply) {
            self.actions.picking = Some(key(&m));
            self.actions.reply_to = Some(m);
        }
    }
}

#[cfg(test)]
mod tests {
    // not `super::*`: GPUI's glob brings its own `test` macro
    use super::{can_delete, can_edit, last_editable, menu, mine, pill, removes, Item, PillEntry, CHANGE_WINDOW_MS};
    use sig_core::projection::{BodyRange, Message, MessageKey, MessageKind, MessageStatus, Reaction, TextStyle};

    const ME: &str = "me";
    const NOW: i64 = 1_800_000_000_000;

    fn message(author: &str, status: MessageStatus, ago_ms: i64) -> Message {
        Message {
            conversation_id: "g:01".into(),
            author: author.into(),
            sent_at: NOW - ago_ms,
            received_at: NOW - ago_ms,
            body: Some("hello".into()),
            ranges: vec![],
            is_outgoing: author == ME,
            status,
            edited: false,
            deleted: false,
            quote: None,
            kind: MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
            story: None,
        }
    }

    fn ours(ago_ms: i64) -> Message {
        message(ME, MessageStatus::Delivered, ago_ms)
    }

    fn items(m: &Message) -> Vec<Item> {
        menu(m, Some(ME), NOW)
    }

    #[test]
    fn our_sent_message_offers_everything_and_theirs_no_changes() {
        use Item::*;
        assert_eq!(items(&ours(60_000)), [React, Reply, Edit, Copy, Delete]);
        assert_eq!(
            items(&message("ana", MessageStatus::Read, 60_000)),
            [React, Reply, Copy]
        );
    }

    #[test]
    fn changes_stop_after_a_day_and_until_sent() {
        use Item::*;
        assert_eq!(items(&ours(CHANGE_WINDOW_MS)), [React, Reply, Edit, Copy, Delete]);
        assert_eq!(items(&ours(CHANGE_WINDOW_MS + 1)), [React, Reply, Copy]);
        assert_eq!(items(&message(ME, MessageStatus::Pending, 0)), [React, Reply, Copy]);
        assert_eq!(
            items(&message(ME, MessageStatus::Failed, 0)),
            [React, Reply, Copy, Retry]
        );
        // not ours without knowing who we are
        assert!(!can_delete(&ours(0), None, NOW));
    }

    #[test]
    fn a_deleted_message_offers_nothing() {
        let mut m = ours(0);
        m.deleted = true;
        m.body = None;
        assert_eq!(items(&m), []);
    }

    #[test]
    fn only_plain_text_that_isnt_a_story_reply_is_edited() {
        use Item::*;
        let mut story = ours(0);
        story.story = Some(MessageKey {
            author: "ana".into(),
            sent_at: 1,
        });
        assert_eq!(items(&story), [React, Reply, Copy, Delete]);
        let mut file = ours(0);
        file.body = None;
        assert_eq!(items(&file), [React, Reply, Delete]);
        file.body = Some(String::new());
        assert!(!can_edit(&file, Some(ME), NOW));
        let mut mention = ours(0);
        mention.body = Some("\u{FFFC} hi".into());
        mention.ranges = vec![BodyRange {
            start: 0,
            length: 1,
            style: TextStyle::Mention { aci: "ana".into() },
        }];
        assert_eq!(
            items(&mention),
            [React, Reply, Copy, Delete],
            "the composer can't hold it"
        );
        let mut bold = ours(0);
        bold.ranges = vec![BodyRange {
            start: 0,
            length: 5,
            style: TextStyle::Bold,
        }];
        assert!(!can_edit(&bold, Some(ME), NOW));
    }

    #[test]
    fn up_edits_the_newest_editable_message() {
        let old = ours(2 * CHANGE_WINDOW_MS);
        let mut editable = ours(10_000);
        editable.body = Some("this one".into());
        let theirs = message("ana", MessageStatus::Read, 5_000);
        let sending = message(ME, MessageStatus::Pending, 0);
        let all = [old.clone(), editable.clone(), theirs, sending];
        assert_eq!(last_editable(&all, Some(ME), NOW), Some(&editable));
        assert_eq!(last_editable(&[old], Some(ME), NOW), None);
    }

    fn reaction(reactor: &str, emoji: &str) -> Reaction {
        Reaction {
            reactor: reactor.into(),
            emoji: emoji.into(),
        }
    }

    #[test]
    fn the_pill_counts_each_emoji_most_first_and_marks_ours() {
        let all = [
            reaction("ana", "👍"),
            reaction(ME, "❤️"),
            reaction("bob", "❤️"),
            reaction("cid", "😂"),
        ];
        let entry = |emoji: &str, count, mine| PillEntry {
            emoji: emoji.into(),
            count,
            mine,
        };
        assert_eq!(
            pill(&all, Some(ME)),
            [entry("❤️", 2, true), entry("👍", 1, false), entry("😂", 1, false)]
        );
        assert_eq!(mine(&all, Some(ME)), Some("❤️"));
        assert_eq!(mine(&all, None), None);
    }

    #[test]
    fn choosing_our_reaction_again_takes_it_back() {
        assert!(removes("❤️", Some("❤️")));
        assert!(!removes("👍", Some("❤️")));
        assert!(!removes("👍", None));
    }
}
