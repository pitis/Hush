//! Moving between chats from the keyboard: the Mac app's Chats menu (`HushApp.swift`'s
//! `ChatCommands` and `ChatList.neighbor`), after Desktop's shortcuts.

use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::Conversation;

use super::Hush;

actions!(
    hush,
    [
        FindChat,
        PreviousChat,
        NextChat,
        PreviousUnreadChat,
        NextUnreadChat,
        FocusComposer,
        CloseChat
    ]
);

/// ⌘1–⌘9: the chat at that place in the list.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = hush, no_json)]
pub struct JumpToChat(pub usize);

/// ⌘K, ⌥↑ ⌥↓, ⌥⇧↑ ⌥⇧↓, ⌘1–⌘9, ⇧⌘T and ⇧⌘C; Ctrl for ⌘ off macOS, Alt is ⌥.
pub fn keys() -> Vec<KeyBinding> {
    // not `secondary-`: `KeyBinding::new` doesn't know the system and panics on it
    let (find, composer, close, numbers) = if cfg!(target_os = "macos") {
        (
            "cmd-k",
            "cmd-shift-t",
            "cmd-shift-c",
            [
                "cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8", "cmd-9",
            ],
        )
    } else {
        (
            "ctrl-k",
            "ctrl-shift-t",
            "ctrl-shift-c",
            [
                "ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4", "ctrl-5", "ctrl-6", "ctrl-7", "ctrl-8", "ctrl-9",
            ],
        )
    };
    let mut keys = vec![
        KeyBinding::new(find, FindChat, None),
        KeyBinding::new("alt-up", PreviousChat, None),
        KeyBinding::new("alt-down", NextChat, None),
        KeyBinding::new("alt-shift-up", PreviousUnreadChat, None),
        KeyBinding::new("alt-shift-down", NextUnreadChat, None),
        // over the text fields' own on Linux (another cursor)
        KeyBinding::new("alt-shift-up", PreviousUnreadChat, Some("Input")),
        KeyBinding::new("alt-shift-down", NextUnreadChat, Some("Input")),
        KeyBinding::new(composer, FocusComposer, None),
        KeyBinding::new(close, CloseChat, None),
    ];
    let jump = numbers
        .into_iter()
        .zip(1..)
        .map(|(key, n)| KeyBinding::new(key, JumpToChat(n), None));
    keys.extend(jump);
    keys
}

/// What `keys` and the Chats menu do, wherever the window's focus is (none at first); each lands
/// in the composer, as in Desktop.
pub fn shortcuts(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    on(window, &hush, cx, |app, _: &FindChat, window, cx| {
        app.focus_search(window, cx)
    });
    on(window, &hush, cx, |app, _: &PreviousChat, window, cx| {
        app.step_chat(true, false, window, cx)
    });
    on(window, &hush, cx, |app, _: &NextChat, window, cx| {
        app.step_chat(false, false, window, cx)
    });
    on(window, &hush, cx, |app, _: &PreviousUnreadChat, window, cx| {
        app.step_chat(true, true, window, cx)
    });
    on(window, &hush, cx, |app, _: &NextUnreadChat, window, cx| {
        app.step_chat(false, true, window, cx)
    });
    on(window, &hush, cx, |app, n: &JumpToChat, window, cx| {
        app.jump_to_chat(n.0, window, cx)
    });
    on(window, &hush, cx, |app, _: &FocusComposer, window, cx| {
        app.focus_composer(window, cx)
    });
    on(window, &hush, cx, |app, _: &CloseChat, window, cx| {
        app.close_chat(window, cx)
    });
}

/// `f` on the app for each `A`, wherever the keys are.
pub(super) fn on<A: Action + Clone>(
    window: AnyWindowHandle,
    hush: &WeakEntity<Hush>,
    cx: &mut App,
    f: fn(&mut Hush, &A, &mut Window, &mut Context<Hush>),
) {
    let hush = hush.clone();
    cx.on_action(move |action: &A, cx| {
        let (hush, action) = (hush.clone(), action.clone());
        // once the dispatch is over: it holds the window, which can't be updated inside it
        cx.defer(move |cx| {
            let done = window.update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| f(app, &action, window, cx))
            });
            if let Err(e) = done.and_then(|r| r) {
                tracing::warn!(error = %e, "a shortcut found no window");
            }
        });
    });
}

impl Hush {
    /// ⌥↑/⌥↓, or ⌥⇧ for unread chats: the chat above or below the open one in the list.
    fn step_chat(&mut self, up: bool, unread_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_search() {
            let next = neighbor(&self.listed(), self.selected.as_deref(), up, unread_only);
            self.open_listed(next, window, cx);
        }
    }

    /// ⌘1–⌘9: the chat at place `n` in the list.
    fn jump_to_chat(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_search() {
            let found = n
                .checked_sub(1)
                .and_then(|i| self.listed().get(i).map(|c| c.id.clone()));
            self.open_listed(found, window, cx);
        }
    }

    /// Opens a listed chat, shown in the list, or stays in the open one: either way the keys go to
    /// its composer.
    fn open_listed(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        match id {
            Some(id) => {
                self.reveal_chat(&id);
                self.select(id, window, cx);
            }
            None => self.focus_composer(window, cx),
        }
    }

    /// ⇧⌘T: the open chat's composer gets the keys.
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_search() && self.selected.is_some() {
            self.composer.update(cx, |c, cx| c.focus(window, cx));
        }
    }

    /// ⇧⌘C: no chat is open; what was typed in it stays as its draft.
    fn close_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_search() && self.selected.is_some() {
            self.switch(None, window, cx);
        }
    }

    /// ↑ or ↓ in the search field, but for stepping through messages found: the chat above or
    /// below opens, and the keys stay in the field (`SidebarView.moveInSearch`).
    pub(super) fn step_listed(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if let Some(id) = neighbor(&self.listed(), self.selected.as_deref(), up, false) {
            self.reveal_chat(&id);
            self.select(id, window, cx);
            self.focus_search(window, cx);
        }
    }

    /// Scrolls the chat list to the row of `id`, which keys can open out of view.
    fn reveal_chat(&self, id: &str) {
        let shown = self.listed();
        let Some(ix) = shown.iter().position(|c| c.id == id) else {
            return;
        };
        // the titles above it: "Chats" while searching; else "Pinned", and "Chats" after those
        let pinned = self.query.is_empty() && shown.iter().any(|c| c.pinned_order.is_some());
        let titles = match () {
            _ if !self.query.is_empty() => 1,
            _ if pinned && shown[ix].pinned_order.is_none() => 2,
            _ if pinned => 1,
            _ => 0,
        };
        self.find.scroll.scroll_to_item(ix + titles);
    }
}

/// The chat above (`up`) or below `selected`, as Desktop's ⌥↑/⌥↓: round the ends of the list;
/// with `unread_only` (⌥⇧↑/⌥⇧↓) the nearest unread chat that way, not round. With none open,
/// the last or the first (`ChatList.neighbor`).
fn neighbor(list: &[&Conversation], selected: Option<&str>, up: bool, unread_only: bool) -> Option<String> {
    let unread = |c: &&&Conversation| c.unread_count > 0 || c.marked_unread;
    let at = selected.and_then(|id| list.iter().position(|c| c.id == id));
    let found = match at {
        None if unread_only && up => list.iter().rev().find(unread),
        None if unread_only => list.iter().find(unread),
        None if up => list.last(),
        None => list.first(),
        Some(i) if unread_only && up => list[..i].iter().rev().find(unread),
        Some(i) if unread_only => list[i + 1..].iter().find(unread),
        Some(i) if up => list.get(i.checked_sub(1).unwrap_or(list.len() - 1)),
        Some(i) => list.get(i + 1).or(list.first()),
    };
    found.map(|c| c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::super::tests::chat;
    use super::neighbor;
    use sig_core::projection::Conversation;

    #[test]
    fn next_and_previous_go_round_but_unread_ones_stop_at_the_ends() {
        let unread = |id: &str, n: u32| Conversation {
            unread_count: n,
            ..chat(id, "")
        };
        let chats = [chat("c:1", ""), unread("c:2", 3), chat("c:3", ""), unread("c:4", 1)];
        let list: Vec<&Conversation> = chats.iter().collect();
        let go = |from: Option<&str>, up: bool, unread: bool| neighbor(&list, from, up, unread);
        assert_eq!(go(Some("c:1"), false, false).as_deref(), Some("c:2"));
        assert_eq!(go(Some("c:4"), false, false).as_deref(), Some("c:1"));
        assert_eq!(go(Some("c:1"), true, false).as_deref(), Some("c:4"));
        assert_eq!(go(None, false, false).as_deref(), Some("c:1"));
        assert_eq!(go(None, true, false).as_deref(), Some("c:4"));
        assert_eq!(go(Some("c:1"), false, true).as_deref(), Some("c:2"));
        assert_eq!(go(Some("c:2"), false, true).as_deref(), Some("c:4"));
        assert_eq!(go(Some("c:4"), false, true), None);
        assert_eq!(go(Some("c:4"), true, true).as_deref(), Some("c:2"));
        assert_eq!(go(Some("c:2"), true, true), None);
        assert_eq!(go(None, true, true).as_deref(), Some("c:4"));
        // the open chat isn't listed (a search that doesn't match it): as if none were open
        assert_eq!(go(Some("g:x"), false, false).as_deref(), Some("c:1"));
        assert_eq!(neighbor(&[], None, false, false), None);
    }

    #[test]
    fn a_chat_marked_unread_counts_as_unread() {
        let marked = Conversation {
            marked_unread: true,
            ..chat("c:u", "")
        };
        let chats = [chat("c:a", ""), marked];
        let list: Vec<&Conversation> = chats.iter().collect();
        assert_eq!(neighbor(&list, Some("c:a"), false, true).as_deref(), Some("c:u"));
    }
}

/// The shortcuts in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::WindowOptions;
    use gpui_kit::{gpui, AnyWindowHandle, App, AppContext as _, Entity, FocusHandle, Focusable as _, TestAppContext};

    use super::super::Hush;
    use super::{shortcuts, NextChat};
    use crate::demo::BOGDAN;
    use crate::Demo;

    fn open(screen: Demo, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(screen, window, cx))
            })
            .expect("the window opens");
            shortcuts(window, hush.downgrade(), cx);
            super::super::search::shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        opened
    }

    fn open_chat(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.selected.clone())
    }

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

    fn composer(app: &Hush, cx: &App) -> FocusHandle {
        app.composer.focus_handle(cx)
    }

    fn search(app: &Hush, cx: &App) -> FocusHandle {
        app.search.focus_handle(cx)
    }

    #[gpui::test]
    fn the_arrows_with_alt_move_through_the_chats_into_their_composer(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        let ana = format!("c:{}", "11111111-1111-4111-8111-111111111111");
        // the list: Family (pinned, unread), Ana, Bogdan, Note to Self, Book club (unread), a request
        cx.simulate_keystrokes(window, "alt-down");
        assert_eq!(open_chat(&hush, cx), Some(ana.clone()));
        assert!(focused(window, &hush, composer, cx));
        cx.simulate_keystrokes(window, "alt-up alt-up");
        assert_eq!(
            open_chat(&hush, cx).as_deref(),
            Some("c:33333333-3333-4333-8333-333333333333")
        );

        // unread ones: from Ana down to Book club, then none further
        cx.dispatch_action(window, NextChat);
        cx.simulate_keystrokes(window, "alt-down");
        assert_eq!(open_chat(&hush, cx), Some(ana));
        cx.simulate_keystrokes(window, "alt-shift-down");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:02"));
        cx.simulate_keystrokes(window, "alt-shift-down");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:02"));
        cx.simulate_keystrokes(window, "alt-shift-up");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:01"));
    }

    #[gpui::test]
    fn command_and_a_number_opens_the_chat_at_that_place(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.search.update(cx, |s, cx| s.focus(window, cx)))
            })
            .unwrap();
        cx.simulate_keystrokes(window, "secondary-3");
        assert_eq!(open_chat(&hush, cx), Some(format!("c:{BOGDAN}")));
        assert!(focused(window, &hush, composer, cx), "from the search field");
        // only six chats are listed
        cx.simulate_keystrokes(window, "secondary-9");
        assert_eq!(open_chat(&hush, cx), Some(format!("c:{BOGDAN}")));
    }

    #[gpui::test]
    fn find_chat_focus_composer_and_close_chat(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_keystrokes(window, "secondary-k");
        assert!(focused(window, &hush, search, cx), "⌘K");
        cx.simulate_keystrokes(window, "secondary-shift-t");
        assert!(focused(window, &hush, composer, cx), "⇧⌘T");

        // what was typed stays as the chat's draft
        cx.simulate_input(window, "half-written");
        cx.simulate_keystrokes(window, "secondary-shift-c");
        assert_eq!(open_chat(&hush, cx), None);
        let draft = hush.read_with(cx, |app, _| app.drafts.get("g:01").cloned());
        assert_eq!(draft.as_deref(), Some("half-written"));
        // nothing to focus or close now
        cx.simulate_keystrokes(window, "secondary-shift-t secondary-shift-c");
        assert!(!focused(window, &hush, composer, cx));
        assert_eq!(open_chat(&hush, cx), None);
    }

    #[gpui::test]
    fn the_shortcuts_wait_for_the_dialog(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::NewGroup, cx);
        cx.simulate_keystrokes(window, "alt-down secondary-2 secondary-shift-c");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:01"));
    }

    #[gpui::test]
    fn the_arrows_in_the_search_field_move_through_the_chats_found(cx: &mut TestAppContext) {
        let (window, hush) = open(Demo::Chat, cx);
        cx.simulate_keystrokes(window, "secondary-k");
        // Bogdan, Book club and the archived Book swap; the open Family isn't one of them
        cx.simulate_input(window, "bo");
        cx.simulate_keystrokes(window, "down");
        assert_eq!(open_chat(&hush, cx), Some(format!("c:{BOGDAN}")));
        cx.simulate_keystrokes(window, "down down");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:03"));
        cx.simulate_keystrokes(window, "down");
        assert_eq!(open_chat(&hush, cx), Some(format!("c:{BOGDAN}")), "round the end");
        cx.simulate_keystrokes(window, "up");
        assert_eq!(open_chat(&hush, cx).as_deref(), Some("g:03"));
        // the search goes on
        assert!(focused(window, &hush, search, cx));
        assert_eq!(hush.read_with(cx, |app, _| app.query.clone()), "bo");
    }
}
