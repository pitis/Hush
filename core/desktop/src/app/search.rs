//! The sidebar search: the chats whose name matches and the messages found, everywhere or in one
//! chat (⇧⌘F), and a found message opened in its chat (the Mac app's `SidebarView.swift`,
//! `Search.swift` and `AppStore`'s search).

use std::ops::Range;
use std::time::Duration;

use async_compat::Compat;
use gpui_kit::component::h_flex;
use gpui_kit::component::v_flex;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, MessageKey, SearchHit, HIGHLIGHT_END, HIGHLIGHT_START};

use super::view::{avatar, icon, label, section};
use super::{describe, Hush, Screen, PAGE};
use crate::style::Palette;
use crate::text;

actions!(hush, [Search, SearchInChat]);

/// Results asked for at a time.
const LIMIT: u32 = 50;
/// How far back a found message can be shown: the open chat is loaded up to it.
const JUMP_LIMIT: u32 = 10_000;
/// Typing pauses this long before the text is looked up.
const PAUSE: Duration = Duration::from_millis(150);
/// How long the opened message stays highlighted.
const HIGHLIGHT: Duration = Duration::from_secs(2);

/// The search's messages; its text is `Hush::search`.
#[derive(Default)]
pub(super) struct Find {
    /// Searching only this chat, or every chat.
    scope: Option<String>,
    hits: Vec<SearchHit>,
    /// A search for the current text is on its way (no results meanwhile, never stale ones).
    pending: bool,
    failed: Option<String>,
    /// The running search: a newer one drops it.
    task: Option<Task<()>>,
    /// The result picked with a click or the arrows.
    picked: Option<usize>,
    /// Bumped by each result opened: only the latest one moves the timeline.
    opening: u64,
    /// The opened result's message, scrolled to once its chat's messages are in.
    jump: Option<MessageKey>,
    /// The opened result's message, highlighted for a moment.
    found: Option<MessageKey>,
    /// The chat list's scrolling, to keep the picked result in view.
    pub(super) scroll: ScrollHandle,
}

/// ⌘F and ⇧⌘F (Ctrl elsewhere); bound before the menus are set, which show them.
pub fn keys() -> [KeyBinding; 2] {
    // `secondary-` is for keymap files: `KeyBinding::new` doesn't know the system and panics on it
    let (find, in_chat) = if cfg!(target_os = "macos") {
        ("cmd-f", "cmd-shift-f")
    } else {
        ("ctrl-f", "ctrl-shift-f")
    };
    [
        KeyBinding::new(find, Search, None),
        KeyBinding::new(in_chat, SearchInChat, None),
    ]
}

/// What `keys` do, wherever the window's focus is (none at first): the window's views see a
/// shortcut only on the way from the focused element.
pub fn shortcuts(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    let on = move |hush: WeakEntity<Hush>, f: fn(&mut Hush, &mut Window, &mut Context<Hush>), cx: &mut App| {
        // once the dispatch is over: it holds the window, which can't be updated inside it
        cx.defer(move |cx| {
            let done = window.update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)));
            if let Err(e) = done.and_then(|r| r) {
                tracing::warn!(error = %e, "a search shortcut found no window");
            }
        });
    };
    let weak = hush.clone();
    cx.on_action(move |_: &Search, cx| on(weak.clone(), Hush::focus_search, cx));
    cx.on_action(move |_: &SearchInChat, cx| on(hush.clone(), Hush::search_in_chat, cx));
}

impl Hush {
    /// The sidebar is there, with no dialog over it.
    fn can_search(&self) -> bool {
        matches!(self.screen, Screen::Chats) && self.dialog.is_none()
    }

    /// The sidebar's search field gets the keys.
    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_search() {
            self.search.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// Searches the open chat only; nothing with no chat open (the Mac app's item is disabled).
    fn search_in_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone().filter(|_| self.can_search()) else {
            return;
        };
        if self.find.scope.as_ref() != Some(&id) {
            self.find.scope = Some(id);
            self.schedule_search(cx);
            cx.notify();
        }
        self.focus_search(window, cx);
    }

    /// "All Chats", or leaving the field empty: the search and its chat scope end.
    pub(super) fn end_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.scope = None;
        self.clear_search(window, cx);
    }

    /// Esc in the search field: its text goes (the field clears itself); once it is empty, the
    /// search ends and the keys go back to the open chat.
    pub(super) fn escape_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.read(cx).value().is_empty() {
            return;
        }
        cx.stop_propagation();
        self.end_search(window, cx);
        if self.selected.is_some() {
            self.composer.update(cx, |c, cx| c.focus(window, cx));
        } else {
            window.blur(cx);
        }
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.query.clear();
        self.schedule_search(cx);
        cx.notify();
    }

    /// Looks the text up once typing pauses; results of an older text or scope never show under
    /// this one.
    pub(super) fn schedule_search(&mut self, cx: &mut Context<Self>) {
        let find = &mut self.find;
        find.hits.clear();
        find.picked = None;
        find.failed = None;
        find.task = None;
        find.pending = false;
        let query = self.search.read(cx).value().trim().to_string();
        let Some(client) = self.client.clone().filter(|_| !query.is_empty()) else {
            return;
        };
        find.pending = true;
        let scope = find.scope.clone();
        find.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE).await;
            let found = Compat::new(client.search(query, scope, LIMIT)).await;
            let _ = this.update(cx, |app, cx| {
                app.find.pending = false;
                match found {
                    Ok(hits) => {
                        let authors = hits.iter().filter(|h| !h.message.is_outgoing);
                        let authors = authors.map(|h| h.message.author.clone()).collect();
                        app.find.hits = hits;
                        app.load_names(authors, cx);
                    }
                    Err(e) => app.find.failed = Some(describe(&e)),
                }
                cx.notify();
            });
        }));
    }

    /// A result's message changed: one deleted for everyone leaves the results, its text with it.
    pub(super) fn recheck_hit(&mut self, key: &MessageKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if !self.find.hits.iter().any(|h| h.message.key() == *key) {
            return;
        }
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            // one that didn't read stays: that isn't a deletion
            let Ok(now) = Compat::new(client.message(key.clone())).await else {
                return;
            };
            if now.is_none_or(|m| m.deleted) {
                let _ = this.update(cx, |app, cx| {
                    app.forget_hit(&key);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// A message gone for good leaves no result behind.
    pub(super) fn forget_hit(&mut self, key: &MessageKey) {
        let before = self.find.hits.len();
        self.find.hits.retain(|h| h.message.key() != *key);
        if self.find.hits.len() != before {
            self.find.picked = None;
        }
    }

    pub(super) fn listed(&self) -> Vec<&Conversation> {
        listed(
            &self.chats,
            &self.query,
            self.find.scope.is_some(),
            self.own_aci.as_deref(),
        )
    }

    /// The arrows step through the messages found, rather than the chats: searching one chat, or
    /// no chat matches.
    fn browsing_results(&self) -> bool {
        !self.query.is_empty() && (self.find.scope.is_some() || self.listed().is_empty())
    }

    /// ↑ or ↓ in the search field while browsing results: the next one opens. Else the field has
    /// the key.
    pub(super) fn step_results(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.browsing_results() || self.find.hits.is_empty() {
            return;
        }
        cx.stop_propagation();
        let next = step(self.find.picked, by, self.find.hits.len());
        // browsing, the list holds the scope row, the "Messages" title, then the results
        let row = usize::from(self.find.scope.is_some()) + 1 + next;
        self.find.scroll.scroll_to_item(row);
        self.open_hit(next, window, cx);
    }

    /// ↩ in the search: the picked result (or the newest) when no chat is listed; else the open
    /// chat if it matches, or the first match, and the search ends.
    pub(super) fn submit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listed = self.listed();
        if self.find.scope.is_some() || listed.is_empty() {
            let ix = self.find.picked.filter(|&i| i < self.find.hits.len()).unwrap_or(0);
            if ix < self.find.hits.len() {
                self.open_hit(ix, window, cx);
            }
            return;
        }
        let Some(id) = chat_to_open(&listed, self.selected.as_deref()) else {
            return;
        };
        // into its composer
        self.select(id, window, cx);
        self.clear_search(window, cx);
    }

    /// Opens a found message: its chat, loaded back to it, scrolled to it and highlighted. The
    /// search field keeps the keys it had.
    fn open_hit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.find.hits.get(ix) else { return };
        let (id, key) = (hit.message.conversation_id.clone(), hit.message.key());
        self.find.picked = Some(ix);
        let typing = self.search.focus_handle(cx).is_focused(window);
        if self.selected.as_ref() != Some(&id) {
            self.select(id.clone(), window, cx);
        }
        if typing {
            self.focus_search(window, cx);
        }
        cx.notify();
        let Some(client) = self.client.clone() else { return };
        self.find.opening += 1;
        let (generation, opening) = (self.generation, self.find.opening);
        cx.spawn(async move |this, cx| {
            let newer = Compat::new(client.count_newer(id, key.clone())).await;
            let _ = this.update(cx, |app, cx| {
                // another chat or another result was opened meanwhile
                if app.generation != generation || app.find.opening != opening {
                    return;
                }
                match newer.map(depth) {
                    Ok(Some(depth)) => {
                        app.depth = depth;
                        app.find.jump = Some(key);
                        // a load of the newest messages still on its way is read again this deep
                        app.reload_messages(cx);
                    }
                    Ok(None) => app.error = Some("That message is too far back to show yet.".into()),
                    Err(e) => app.error = Some(format!("Couldn't open that message: {}", describe(&e))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The open chat's messages didn't load: a found message waiting for them isn't scrolled to
    /// by a later load.
    pub(super) fn messages_failed(&mut self) {
        self.find.jump = None;
    }

    /// Back at the newest message after a found one was shown: only the newest page loads again
    /// (the rows back to it are off screen by then), so a deep jump isn't reloaded and redrawn
    /// for as long as the chat stays open.
    pub(super) fn back_to_newest(&mut self, cx: &mut Context<Self>) {
        let at_end = !self.timeline.read(cx).is_scrolled_up();
        if self.depth > PAGE && self.find.jump.is_none() && at_end {
            self.depth = PAGE;
            self.reload_messages(cx);
        }
    }

    /// Once the opened message's chat is loaded: to it, highlighted for a moment.
    pub(super) fn reveal_found(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.find.jump.take() else { return };
        // gone meanwhile (deleted, expired): the chat shows as it is
        let Some(ix) = self.messages.iter().position(|m| m.key() == key) else {
            return;
        };
        // shortcut: the message goes to the top of the timeline (the Mac app centres it):
        // MessageScrollerState scrolls only to a row's top
        self.timeline.update(cx, |t, cx| t.scroll_to_item(ix, cx));
        self.find.found = Some(key.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT).await;
            let _ = this.update(cx, |app, cx| {
                if app.find.found.as_ref() == Some(&key) {
                    app.find.found = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn is_found(&self, key: &MessageKey) -> bool {
        self.find.found.as_ref() == Some(key)
    }

    /// Searching one chat: which, and the way back to every chat.
    pub(super) fn render_scope(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let scope = self.find.scope.as_ref()?;
        let chat = self.chats.iter().find(|c| &c.id == scope);
        let title = chat.map_or("this chat".into(), |c| text::title(c, self.own_aci.as_deref()));
        Some(
            h_flex()
                .px(px(20.))
                .pt(px(6.))
                .pb(px(4.))
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(p.secondary)
                .child(icon("text-search", 12., p.secondary))
                .child(
                    label("search-scope", Role::Label, format!("In {title}"))
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(
                    div()
                        .id("all-chats")
                        .role(Role::Button)
                        .aria_label("All Chats")
                        .flex_none()
                        .cursor_pointer()
                        .text_color(p.accent)
                        .child("All Chats")
                        .on_click(cx.listener(|this, _, window, cx| this.end_search(window, cx))),
                ),
        )
    }

    /// The messages found, after the chats: their rows, or how the search is doing.
    pub(super) fn render_results(&self, p: Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.query.is_empty() {
            return vec![];
        }
        let find = &self.find;
        if find.hits.is_empty() {
            let (said, color) = match (find.pending, &find.failed) {
                (true, _) => ("Searching…".to_string(), p.secondary),
                (false, Some(why)) => (format!("Search failed: {why}"), p.warning),
                (false, None) => ("No messages found".into(), p.secondary),
            };
            let status = label("search-status", Role::Status, said)
                .px(px(20.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(color);
            return vec![status.into_any_element()];
        }
        let now = chrono::Local::now();
        let rows = find.hits.iter().enumerate().map(|(ix, hit)| {
            let m = &hit.message;
            let chat = self.chats.iter().find(|c| c.id == m.conversation_id);
            let title = chat.map_or_else(
                || text::short_id(m.conversation_id.get(2..).unwrap_or_default()),
                |c| text::title(c, self.own_aci.as_deref()),
            );
            // a blocked chat's or a request's picture isn't shown (a stranger chose it)
            let picture = chat
                .filter(|c| !c.blocked && !c.message_request)
                .and_then(|c| c.avatar.as_deref())
                .and_then(|path| self.picture(path));
            let sender = if m.is_outgoing {
                "You".into()
            } else {
                self.name(&m.author)
            };
            let who = who(&m.conversation_id, m.is_outgoing, &sender);
            let (found, matched) = snippet(&hit.snippet);
            let line = format!("{who}{found}");
            let time = text::list_time(m.sent_at, now);
            let picked = find.picked == Some(ix);
            let quiet = HighlightStyle {
                color: Some(p.secondary),
                ..Default::default()
            };
            let bold = HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            };
            let shift = |r: Range<usize>| r.start + who.len()..r.end + who.len();
            let highlights: Vec<_> = (!who.is_empty())
                .then_some((0..who.len(), quiet))
                .into_iter()
                .chain(matched.into_iter().map(|r| (shift(r), bold)))
                .collect();
            div()
                .id(("hit", ix))
                .role(Role::ListItem)
                .aria_label(format!("{title}. {line}. {time}"))
                .aria_selected(picked)
                .px(px(10.))
                .on_click(cx.listener(move |this, _, window, cx| this.open_hit(ix, window, cx)))
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(12.))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .when(picked, |this| this.bg(p.selection))
                        .child(avatar(&m.conversation_id, &title, picture, 36.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(
                                    h_flex()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(title),
                                        )
                                        .child(
                                            div().flex_none().text_size(px(11.)).text_color(p.secondary).child(time),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .line_clamp(2)
                                        .text_ellipsis()
                                        .child(StyledText::new(line).with_highlights(highlights)),
                                ),
                        ),
                )
                .into_any_element()
        });
        std::iter::once(section("Messages", p).into_any_element())
            .chain(rows)
            .collect()
    }

    /// `--snapshot`'s search screens: every chat searched for "book", or the open one for
    /// "bring" with its newest match opened.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo_search(&mut self, in_chat: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (query, hits) = if in_chat {
            self.find.scope = self.selected.clone();
            ("bring", crate::demo::found(&self.messages, "bring"))
        } else {
            ("book", crate::demo::hits())
        };
        self.search.update(cx, |s, cx| s.set_value(query, window, cx));
        self.query = query.into();
        self.find.hits = hits;
        if in_chat {
            self.find.picked = Some(0);
            self.find.found = self.find.hits.first().map(|h| h.message.key());
        }
    }
}

/// Case-insensitive, or the query's letters in order ("hbh" matches "He Be Hatin"); blank
/// matches all (`Formatting.matches`).
fn matches(text: &str, query: &str) -> bool {
    let (text, query) = (text.to_lowercase(), query.trim().to_lowercase());
    if text.contains(&query) {
        return true;
    }
    let mut rest = text.chars();
    query.chars().filter(|c| *c != ' ').all(|q| rest.any(|t| t == q))
}

/// The chats the sidebar lists: the inbox; while searching, every chat whose name matches,
/// archived ones too; none while searching one chat (`AppStore.visibleConversations`).
fn listed<'a>(chats: &'a [Conversation], query: &str, in_chat: bool, own_aci: Option<&str>) -> Vec<&'a Conversation> {
    match () {
        _ if in_chat => vec![],
        _ if query.trim().is_empty() => chats.iter().filter(|c| !c.archived).collect(),
        _ => chats
            .iter()
            .filter(|c| matches(&text::title(c, own_aci), query))
            .collect(),
    }
}

/// A snippet without the core's match markers, and where the matches are (to draw bold).
fn snippet(marked: &str) -> (String, Vec<Range<usize>>) {
    let mut text = String::with_capacity(marked.len());
    let mut found = vec![];
    let mut start = None;
    for c in marked.chars() {
        match c {
            HIGHLIGHT_START => start = Some(text.len()),
            HIGHLIGHT_END => found.extend(start.take().map(|s| s..text.len())),
            c => text.push(c),
        }
    }
    found.extend(start.map(|s| s..text.len()));
    found.retain(|r| !r.is_empty());
    (text, found)
}

/// Who wrote a found message, before its text: in a group or ours.
fn who(conversation_id: &str, outgoing: bool, sender: &str) -> String {
    if conversation_id.starts_with("g:") || outgoing {
        format!("{sender}: ")
    } else {
        String::new()
    }
}

/// The result an arrow picks: the next one, stopping at the ends; the first or the last when none
/// is picked yet (`SidebarView.moveInSearch`). `count` is above 0.
fn step(picked: Option<usize>, by: isize, count: usize) -> usize {
    match picked {
        Some(ix) => ix.saturating_add_signed(by).min(count - 1),
        None if by > 0 => 0,
        None => count - 1,
    }
}

/// How many of a chat's newest messages to load to show one with `newer` after it, with some
/// before it; none when it is too far back.
fn depth(newer: u32) -> Option<u32> {
    (newer < JUMP_LIMIT).then(|| PAGE.max(newer + 50))
}

/// The chat ↩ opens: the open one if it is listed, else the first listed.
fn chat_to_open(listed: &[&Conversation], selected: Option<&str>) -> Option<String> {
    let open = listed.iter().find(|c| Some(c.id.as_str()) == selected);
    open.or(listed.first()).map(|c| c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::super::tests::chat;
    use super::{chat_to_open, depth, listed, matches, snippet, step, who, PAGE};
    use sig_core::projection::Conversation;

    #[test]
    fn a_chat_matches_by_part_of_its_name_or_its_letters_in_order() {
        assert!(matches("He Be Hatin", ""));
        assert!(matches("He Be Hatin", " be "));
        assert!(matches("He Be Hatin", "hbh"));
        assert!(matches("He Be Hatin", "HE HAT"));
        assert!(!matches("He Be Hatin", "hhb"));
        assert!(matches("Ioana Ștefan", "ște"));
        assert!(!matches("Ana", "anna"));
    }

    #[test]
    fn searching_lists_every_chat_whose_name_matches_archived_ones_too() {
        let archived = Conversation {
            archived: true,
            ..chat("g:2", "Book swap")
        };
        let all = [chat("g:1", "Book club"), chat("c:a", "Ana"), archived];
        let ids = |shown: Vec<&Conversation>| -> Vec<String> { shown.into_iter().map(|c| c.id.clone()).collect() };
        assert_eq!(ids(listed(&all, "", false, None)), ["g:1", "c:a"]);
        assert_eq!(ids(listed(&all, "book", false, None)), ["g:1", "g:2"]);
        assert_eq!(
            ids(listed(&all, "  ", false, None)),
            ["g:1", "c:a"],
            "spaces alone aren't a search"
        );
        // searching one chat lists none, typed or not
        assert!(listed(&all, "book", true, None).is_empty());
        assert!(listed(&all, "", true, None).is_empty());
        // our own chat by the name it shows
        assert_eq!(ids(listed(&[chat("c:me", "")], "note", false, Some("me"))), ["c:me"]);
    }

    #[test]
    fn a_snippets_matches_are_found_and_their_markers_dropped() {
        let (text, bold) = snippet("…the \u{E000}book\u{E001}ed table, \u{E000}Bö\u{E001}");
        assert_eq!(text, "…the booked table, Bö");
        let found: Vec<&str> = bold.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, ["book", "Bö"]);
        assert_eq!(snippet("no match"), ("no match".into(), vec![]));
        // an unclosed match runs to the end
        let (text, bold) = snippet("a \u{E000}bc");
        assert_eq!(&text[bold[0].clone()], "bc");
    }

    #[test]
    fn the_writer_is_said_in_groups_and_for_our_own() {
        assert_eq!(who("g:1", false, "Ana"), "Ana: ");
        assert_eq!(who("c:a", true, "You"), "You: ");
        assert_eq!(who("c:a", false, "Ana"), "");
    }

    #[test]
    fn the_arrows_step_through_results_and_stop_at_the_ends() {
        assert_eq!(step(None, 1, 3), 0);
        assert_eq!(step(None, -1, 3), 2);
        assert_eq!(step(Some(0), 1, 3), 1);
        assert_eq!(step(Some(2), 1, 3), 2);
        assert_eq!(step(Some(0), -1, 3), 0);
        // the results changed under a pick: it stays in them
        assert_eq!(step(Some(7), -1, 3), 2);
    }

    #[test]
    fn a_found_message_loads_with_some_before_it_unless_too_far_back() {
        assert_eq!(depth(0), Some(PAGE));
        assert_eq!(depth(400), Some(450));
        assert_eq!(depth(9_999), Some(10_049));
        assert_eq!(depth(10_000), None);
    }

    #[test]
    fn return_opens_the_open_chat_if_listed_else_the_first() {
        let (a, b) = (chat("c:a", "Ana"), chat("c:b", "Bogdan"));
        let shown = [&a, &b];
        assert_eq!(chat_to_open(&shown, Some("c:b")).as_deref(), Some("c:b"));
        assert_eq!(chat_to_open(&shown, Some("g:x")).as_deref(), Some("c:a"));
        assert_eq!(chat_to_open(&shown, None).as_deref(), Some("c:a"));
        assert_eq!(chat_to_open(&[], Some("c:a")), None);
    }
}

/// The search in a real (headless) window: keys and actions go through GPUI's dispatch. Needs GPUI's
/// test support, which `snapshot` turns on: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::WindowOptions;
    use gpui_kit::{gpui, AnyWindowHandle, App, AppContext as _, Entity, FocusHandle, Focusable as _, TestAppContext};

    use super::super::{Hush, PAGE};
    use super::{keys, shortcuts, Search, SearchInChat};
    use crate::Demo;

    fn open(screen: Demo, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(keys());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        opened
    }

    /// Whether `field` of the app has the keys.
    fn focused(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        field: fn(&Hush, &App) -> FocusHandle,
        cx: &mut TestAppContext,
    ) -> bool {
        window
            .update(cx, |_, window, cx| field(hush.read(cx), cx).is_focused(window))
            .expect("the window is open")
    }

    fn search(app: &Hush, cx: &App) -> FocusHandle {
        app.search.focus_handle(cx)
    }

    fn scope(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.find.scope.clone())
    }

    #[gpui::test]
    fn search_shortcuts_and_menu_items_reach_the_field_from_anywhere(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        // nothing has the keys yet
        cx.simulate_keystrokes(window, "secondary-f");
        assert!(focused(window, &hush, search, cx), "⌘F with nothing focused");

        // from the composer, whose own find is off
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.composer.update(cx, |c, cx| c.focus(window, cx)))
            })
            .unwrap();
        cx.simulate_keystrokes(window, "secondary-f");
        assert!(focused(window, &hush, search, cx), "⌘F from the composer");

        // ⇧⌘F: the open chat only
        cx.simulate_keystrokes(window, "secondary-shift-f");
        assert_eq!(scope(&hush, cx).as_deref(), Some("g:01"));

        // the menu items dispatch the same actions
        let (window, hush) = open(Demo::Chat, cx);
        cx.dispatch_action(window, Search);
        assert!(focused(window, &hush, search, cx), "the Search menu item");
        cx.dispatch_action(window, SearchInChat);
        assert_eq!(
            scope(&hush, cx).as_deref(),
            Some("g:01"),
            "the Search in Chat menu item"
        );
    }

    #[gpui::test]
    fn search_in_chat_waits_for_the_dialog(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::NewGroup, cx);
        cx.dispatch_action(window, SearchInChat);
        assert_eq!(scope(&hush, cx), None, "the sidebar behind the dialog stays as it is");
    }

    #[gpui::test]
    fn escape_clears_the_search_then_ends_it(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_keystrokes(window, "secondary-shift-f");
        cx.simulate_input(window, "pot");
        assert_eq!(hush.read_with(cx, |app, _| app.query.clone()), "pot");

        // with text: the text goes, the chat search stays
        cx.simulate_keystrokes(window, "escape");
        assert_eq!(hush.read_with(cx, |app, _| app.query.clone()), "");
        assert_eq!(scope(&hush, cx).as_deref(), Some("g:01"));
        assert!(focused(window, &hush, search, cx));

        // empty: the search ends, and the keys go back to the chat
        cx.simulate_keystrokes(window, "escape");
        assert_eq!(scope(&hush, cx), None);
        assert!(focused(window, &hush, |app, cx| app.composer.focus_handle(cx), cx));
    }

    #[gpui::test]
    fn back_at_the_newest_message_a_chat_loads_only_its_newest_again(cx: &mut TestAppContext) {
        let (_, hush) = open(Demo::Chat, cx);
        let notify = |hush: &Entity<Hush>, cx: &mut TestAppContext| {
            hush.update(cx, |app, cx| app.timeline.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        };
        // a found message is still on its way: its load is kept
        hush.update(cx, |app, _| {
            app.depth = 500;
            app.find.jump = Some(app.messages[0].key());
        });
        notify(&hush, cx);
        assert_eq!(hush.read_with(cx, |app, _| app.depth), 500);

        // shown, and the timeline is at its end again (the demo chat fits the window)
        hush.update(cx, |app, _| app.find.jump = None);
        notify(&hush, cx);
        assert_eq!(hush.read_with(cx, |app, _| app.depth), PAGE);
    }
}
