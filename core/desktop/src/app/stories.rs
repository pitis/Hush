//! Stories (the Mac app's `StoriesView.swift`, `StoryList`): the last day's, by chat, behind the
//! Stories button with the count of new ones; a window over the chats steps through them, each
//! marked seen once shown. They are only shown: the Mac app neither replies to stories nor posts
//! them.

use std::cmp::Reverse;
use std::collections::HashSet;

use async_compat::Compat;
use gpui_kit::*;
use sig_core::projection::{AttachmentState, MessageKey, Story};

use super::Hush;

pub(super) mod view;

type Key = (String, i64);

#[derive(Default)]
pub(super) struct Stories {
    /// The last day's, oldest first, as the core lists them (none from blocked chats or requests).
    list: Vec<Story>,
    loading: bool,
    again: bool,
    /// The window over the chats, while open.
    viewer: Option<Viewer>,
}

impl Stories {
    /// The window is over the chats: the shortcuts wait for it.
    pub(super) fn showing(&self) -> bool {
        self.viewer.is_some()
    }

    /// Off the chats (unlinked, say): the window goes with them.
    pub(super) fn close(&mut self) {
        self.viewer = None;
    }
}

pub(super) struct Viewer {
    /// The chats' order as it opened: seeing a story doesn't move its chat under ←/→.
    order: Vec<String>,
    /// The chat whose stories show, and which of them.
    chat: Option<String>,
    index: usize,
    /// The story shown last: another one in its place is new to see.
    shown: Option<Key>,
    /// The keys go to it; Tab stays inside.
    focus: FocusHandle,
}

/// One chat's stories, oldest first.
pub(super) struct Group<'a> {
    pub chat: &'a str,
    pub stories: Vec<&'a Story>,
}

impl Group<'_> {
    /// One of them isn't seen yet.
    pub fn unseen(&self) -> bool {
        self.stories.iter().any(|s| !s.viewed)
    }

    pub fn newest(&self) -> i64 {
        self.stories.iter().map(|s| s.sent_at).max().unwrap_or(0)
    }

    fn ours(&self, me: Option<&str>) -> bool {
        self.stories.iter().any(|s| Some(s.author.as_str()) == me)
    }
}

fn key(s: &Story) -> Key {
    (s.author.clone(), s.sent_at)
}

/// One group per chat, oldest story first: ours first, then chats with unseen stories, newest
/// first within each (`StoryList.groups`).
fn groups<'a>(stories: &'a [Story], me: Option<&str>) -> Vec<Group<'a>> {
    let mut groups: Vec<Group> = vec![];
    for s in stories {
        match groups.iter_mut().find(|g| g.chat == s.conversation_id) {
            Some(g) => g.stories.push(s),
            None => groups.push(Group {
                chat: &s.conversation_id,
                stories: vec![s],
            }),
        }
    }
    for g in &mut groups {
        g.stories.sort_by_key(|s| s.sent_at);
    }
    groups.sort_by_key(|g| (!g.ours(me), !g.unseen(), Reverse(g.newest())));
    groups
}

/// `groups` in `order`; chats that came since go last, in their own order (`StoryList.ordered`).
fn ordered<'a>(mut groups: Vec<Group<'a>>, order: &[String]) -> Vec<Group<'a>> {
    groups.sort_by_key(|g| order.iter().position(|id| id == g.chat).unwrap_or(usize::MAX));
    groups
}

/// Others' stories not seen yet (`StoryList.unseenCount`).
pub(super) fn unseen_count(stories: &[Story], me: Option<&str>) -> usize {
    let others = stories.iter().filter(|s| Some(s.author.as_str()) != me);
    others.filter(|s| !s.viewed).count()
}

/// The story after (`by` 1) or before (-1) `chat`'s `index`, on into the next chat's first or the
/// previous one's last; None at either end (`StoriesView.step`).
fn step(groups: &[Group], chat: Option<&str>, index: usize, by: isize) -> Option<(String, usize)> {
    let g = groups.iter().position(|g| Some(g.chat) == chat)?;
    if let Some(i) = index.checked_add_signed(by).filter(|&i| i < groups[g].stories.len()) {
        return Some((groups[g].chat.to_owned(), i));
    }
    let other = groups.get(g.checked_add_signed(by)?)?;
    let i = if by > 0 { 0 } else { other.stories.len() - 1 };
    Some((other.chat.to_owned(), i))
}

/// Where in `chat` the viewer is once the list changed: on the story `shown` before, wherever it
/// is now; if it went (it expired), on the one in its place, else its chat's last.
fn kept(groups: &[Group], chat: Option<&str>, index: usize, shown: Option<&Key>) -> usize {
    let Some(g) = groups.iter().find(|g| Some(g.chat) == chat) else {
        return index;
    };
    let now = shown.and_then(|k| g.stories.iter().position(|s| &key(s) == k));
    now.unwrap_or(index.min(g.stories.len() - 1))
}

/// The stories of `before` that `after` hasn't (expired, or their chat blocked).
fn left(before: &[Story], after: &[Story]) -> HashSet<Key> {
    let still: HashSet<Key> = after.iter().map(key).collect();
    before.iter().map(key).filter(|k| !still.contains(k)).collect()
}

impl Hush {
    /// Reloads the stories; calls while a load runs are folded into one more. One that fails keeps
    /// what shows (as on the Mac).
    pub(super) fn refresh_stories(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        if self.stories.loading {
            self.stories.again = true;
            return;
        }
        self.stories.loading = true;
        cx.spawn(async move |this, cx| loop {
            let found = Compat::new(client.stories()).await;
            let again = this.update(cx, |app, cx| {
                match found {
                    Ok(list) => app.set_stories(list, cx),
                    Err(e) => tracing::warn!(error = %e, "the stories couldn't be read"),
                }
                app.stories.loading = std::mem::take(&mut app.stories.again);
                app.stories.loading
            });
            if !again.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    /// The stories as the core lists them now: the viewer stays on the story it showed, and a story
    /// that left takes its opened copy with it.
    fn set_stories(&mut self, list: Vec<Story>, cx: &mut Context<Self>) {
        let gone = left(&self.stories.list, &list);
        self.drop_story_copies(&gone);
        let me = self.own_aci.clone();
        let Stories {
            list: shown, viewer, ..
        } = &mut self.stories;
        *shown = list;
        if let Some(v) = viewer {
            v.index = kept(
                &groups(shown, me.as_deref()),
                v.chat.as_deref(),
                v.index,
                v.shown.as_ref(),
            );
        }
        let authors = shown.iter().map(|s| s.author.clone());
        let others: Vec<String> = authors.filter(|a| Some(a) != me.as_ref()).collect();
        self.load_names(others, cx);
        self.story_shown(cx);
        cx.notify();
    }

    /// A file of `message` changed state: a story's shows it.
    pub(super) fn story_file_changed(&mut self, message: &MessageKey, cx: &mut Context<Self>) {
        let k = (message.author.clone(), message.sent_at);
        if self.stories.list.iter().any(|s| key(s) == k) {
            self.refresh_stories(cx);
        }
    }

    /// The Stories button: the window over the chats, on the first chat's first story.
    pub(super) fn open_stories(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() || self.group.is_some() || self.stories.list.is_empty() {
            return;
        }
        let order: Vec<String> = groups(&self.stories.list, self.own_aci.as_deref())
            .iter()
            .map(|g| g.chat.to_owned())
            .collect();
        let focus = cx.focus_handle();
        // the composer behind doesn't keep the keys
        focus.focus(window, cx);
        self.stories.viewer = Some(Viewer {
            chat: order.first().cloned(),
            order,
            index: 0,
            shown: None,
            focus,
        });
        self.story_shown(cx);
        cx.notify();
    }

    /// Esc or Close: back to the chat's composer.
    pub(super) fn close_stories(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stories.viewer.take().is_some() {
            self.composer.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    /// A chat picked in the list: its first story.
    fn show_story_chat(&mut self, chat: String, cx: &mut Context<Self>) {
        if let Some(v) = &mut self.stories.viewer {
            v.chat = Some(chat);
            v.index = 0;
        }
        self.story_shown(cx);
        cx.notify();
    }

    /// ←/→ and the arrows: the next or previous story, on into the next or previous chat's.
    fn step_story(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(v) = &self.stories.viewer else { return };
        let Some((chat, index)) = step(&self.story_groups(), v.chat.as_deref(), v.index, by) else {
            return;
        };
        if let Some(v) = &mut self.stories.viewer {
            v.chat = Some(chat);
            v.index = index;
        }
        self.story_shown(cx);
        cx.notify();
    }

    /// The viewer's chats, in its order.
    fn story_groups(&self) -> Vec<Group<'_>> {
        let order = self.stories.viewer.as_ref().map_or(&[][..], |v| &v.order);
        ordered(groups(&self.stories.list, self.own_aci.as_deref()), order)
    }

    /// The story the viewer shows.
    pub(super) fn current_story(&self) -> Option<&Story> {
        let v = self.stories.viewer.as_ref()?;
        let chat = groups(&self.stories.list, self.own_aci.as_deref())
            .into_iter()
            .find(|g| Some(g.chat) == v.chat.as_deref())?;
        chat.stories.get(v.index).copied()
    }

    /// Another story shows (opened, stepped to, or in the place of one that went): it is marked
    /// seen, and its file fetched if it waits (only now: not every story is looked at).
    fn story_shown(&mut self, cx: &mut Context<Self>) {
        let Some(story) = self.current_story().cloned() else {
            return;
        };
        let k = key(&story);
        let Some(v) = self.stories.viewer.as_mut().filter(|v| v.shown.as_ref() != Some(&k)) else {
            return;
        };
        v.shown = Some(k.clone());
        let waiting = story
            .file
            .filter(|f| f.available && f.state == AttachmentState::Pending);
        if let Some(file) = waiting {
            self.download(&file, cx);
        }
        let Some(client) = self.client.clone().filter(|_| !story.viewed) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let asked = MessageKey {
                author: k.0.clone(),
                sent_at: k.1,
            };
            let marked = Compat::new(client.story_viewed(asked)).await;
            let _ = this.update(cx, |app, cx| match marked {
                Ok(()) => {
                    if let Some(s) = app.stories.list.iter_mut().find(|s| key(s) == k) {
                        s.viewed = true;
                        cx.notify();
                    }
                }
                // as on the Mac: it shows as new next time
                Err(e) => tracing::warn!(error = %e, "a story couldn't be marked seen"),
            });
        })
        .detach();
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s stories over the Family chat: the window open, `steps` stories on from the
    /// first (ours). Nothing is marked seen: there's no core.
    pub(super) fn demo_stories(&mut self, steps: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (list, photo) = crate::demo::stories();
        self.stories.list = list;
        // the photo's picture is drawn here, not read
        self.files = super::files::Files::demo(vec![photo]);
        self.open_stories(window, cx);
        for _ in 0..steps {
            self.step_story(1, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{groups, kept, left, ordered, step, unseen_count, Group};
    use sig_core::projection::{Story, StoryText};

    const ME: &str = "me";

    fn story(chat: &str, author: &str, sent_at: i64, viewed: bool) -> Story {
        Story {
            conversation_id: chat.into(),
            author: author.into(),
            sent_at,
            text: Some(StoryText {
                text: "hi".into(),
                style: 0,
                foreground: None,
                text_background: None,
                background: vec![],
                angle: 0,
                link: None,
            }),
            file: None,
            viewed,
        }
    }

    fn chats(groups: &[Group]) -> Vec<String> {
        groups.iter().map(|g| g.chat.to_owned()).collect()
    }

    /// The made-up day: ours, Ana's two (one seen), Bogdan's seen one, and one in a group.
    fn day() -> Vec<Story> {
        vec![
            story("c:bogdan", "bogdan", 10, true),
            story("c:ana", "ana", 30, false),
            story("g:family", "carmen", 20, false),
            story("c:ana", "ana", 15, true),
            story("c:me", ME, 5, false),
        ]
    }

    #[test]
    fn stories_go_by_chat_ours_first_then_unseen_then_newest() {
        let all = day();
        let by_chat = groups(&all, Some(ME));
        assert_eq!(chats(&by_chat), ["c:me", "c:ana", "g:family", "c:bogdan"]);
        let ana: Vec<i64> = by_chat[1].stories.iter().map(|s| s.sent_at).collect();
        assert_eq!(ana, [15, 30], "oldest first");
        assert!(by_chat[1].unseen() && !by_chat[3].unseen());
        assert_eq!(by_chat[1].newest(), 30);

        // ours in a group counts as ours
        let mut posted = all.clone();
        posted.push(story("g:club", ME, 1, true));
        assert_eq!(chats(&groups(&posted, Some(ME)))[..2], ["c:me", "g:club"]);
    }

    #[test]
    fn the_new_count_is_others_stories_not_seen() {
        assert_eq!(unseen_count(&day(), Some(ME)), 2, "not ours, not those seen");
        assert_eq!(unseen_count(&[], Some(ME)), 0);
    }

    #[test]
    fn the_order_stays_as_the_window_opened_new_chats_last() {
        let mut all = day();
        let order = chats(&groups(&all, Some(ME)));
        // Ana's seen now, and a new chat came: neither moves the others
        all.iter_mut().for_each(|s| s.viewed = true);
        all.push(story("c:zoe", "zoe", 40, false));
        let now = ordered(groups(&all, Some(ME)), &order);
        assert_eq!(chats(&now), ["c:me", "c:ana", "g:family", "c:bogdan", "c:zoe"]);
    }

    #[test]
    fn the_arrows_step_through_a_chats_stories_on_into_the_next_and_stop_at_the_ends() {
        let all = day();
        let by_chat = groups(&all, Some(ME));
        let go = |chat: &str, index, by| step(&by_chat, Some(chat), index, by);
        assert_eq!(go("c:ana", 0, 1), Some(("c:ana".into(), 1)));
        assert_eq!(go("c:ana", 1, 1), Some(("g:family".into(), 0)), "the next chat's first");
        assert_eq!(
            go("g:family", 0, -1),
            Some(("c:ana".into(), 1)),
            "the previous one's last"
        );
        assert_eq!(go("c:me", 0, -1), None);
        assert_eq!(go("c:bogdan", 0, 1), None);
        assert_eq!(step(&by_chat, None, 0, 1), None);
    }

    #[test]
    fn once_a_story_expires_the_viewer_stays_on_the_one_it_showed_or_the_one_in_its_place() {
        let before = day();
        // on Ana's second (30); her first expires
        let after: Vec<_> = before.iter().filter(|s| s.sent_at != 15).cloned().collect();
        let now = groups(&after, Some(ME));
        let shown = ("ana".to_string(), 30);
        assert_eq!(kept(&now, Some("c:ana"), 1, Some(&shown)), 0, "the same story");
        // the one shown went: the one in its place, else the chat's last
        let gone = ("ana".to_string(), 15);
        assert_eq!(kept(&now, Some("c:ana"), 0, Some(&gone)), 0);
        assert_eq!(kept(&now, Some("c:ana"), 1, Some(&gone)), 0);
        // the chat's last went: nothing shows ("No stories"), as on the Mac
        assert_eq!(kept(&now, Some("c:gone"), 1, Some(&gone)), 1);

        assert_eq!(left(&before, &after), [gone].into());
        assert!(left(&after, &before).is_empty(), "a story come isn't one gone");
    }
}

/// Stories in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext, WindowOptions};

    use super::super::attach::window_tests::with_core;
    use super::super::{files, navigate, Hush};
    use super::{key, Key};
    use crate::demo::ANA;
    use crate::Demo;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            let (window, hush) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Stories(0), window, cx))
            })
            .expect("the window opens");
            navigate::shortcuts(window, hush.downgrade(), cx);
            (window, hush)
        });
        cx.run_until_parked();
        opened
    }

    fn shown(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Option<(Key, bool)> {
        hush.read_with(cx, |app, _| app.current_story().map(|s| (key(s), s.viewed)))
    }

    /// Until `done`, as the core answers from its own threads.
    fn until(hush: &Entity<Hush>, cx: &mut TestAppContext, done: impl Fn(&Hush) -> bool) {
        for _ in 0..200 {
            cx.run_until_parked();
            if hush.read_with(cx, |app, _| done(app)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("it didn't happen");
    }

    #[gpui::test]
    fn the_arrows_step_through_the_stories_each_marked_seen_once_shown(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let data = std::env::temp_dir().join(format!("hush-desktop-stories-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        cx.executor().allow_parking();
        let new = |hush: &Entity<Hush>, cx: &mut TestAppContext| {
            hush.read_with(cx, |app, _| {
                super::unseen_count(&app.stories.list, app.own_aci.as_deref())
            })
        };
        assert_eq!(new(&hush, cx), 3);

        // ours first, then Ana's: shown, it's seen
        cx.simulate_keystrokes(window, "right");
        let (ana, _) = shown(&hush, cx).expect("a story shows");
        assert_eq!(ana.0, ANA);
        until(&hush, cx, |app| app.current_story().is_some_and(|s| s.viewed));
        assert_eq!(new(&hush, cx), 2, "marked seen once shown");
        cx.simulate_keystrokes(window, "left");
        assert_eq!(shown(&hush, cx).map(|(k, _)| k.0).as_deref(), Some(crate::demo::ME));

        // the chats behind wait
        cx.simulate_keystrokes(window, "alt-down");
        assert_eq!(
            hush.read_with(cx, |app, _| app.selected.clone()).as_deref(),
            Some("g:01")
        );
        cx.simulate_keystrokes(window, "escape");
        assert!(hush.read_with(cx, |app, _| app.stories.viewer.is_none()));
        let composer = window
            .update(cx, |_, window, cx| {
                hush.read(cx).composer.focus_handle(cx).is_focused(window)
            })
            .unwrap();
        assert!(composer, "the keys go back to the chat");
        hush.update(cx, |app, _| app.client = None);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_photo_story_is_decoded_once_shown_and_kept_while_the_chat_behind_reloads(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let data = std::env::temp_dir().join(format!("hush-desktop-story-photo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        // a plain file: the core reads one outside its folder as it is
        let photo = data.join("photo.png");
        std::fs::write(&photo, crate::demo::picture()).unwrap();
        let path = photo.to_str().unwrap().to_owned();
        with_core(&hush, &data, cx);
        cx.executor().allow_parking();
        hush.update(cx, |app, cx| {
            let mut list = app.stories.list.clone();
            let photos = list
                .iter_mut()
                .filter_map(|s| s.file.as_mut())
                .filter(|f| files::is_image(f));
            photos.for_each(|f| f.path = Some(path.clone()));
            app.files = Default::default();
            app.set_stories(list, cx);
        });
        // ours, Ana's words, then her photo
        cx.simulate_keystrokes(window, "right right");
        let decoded = |app: &Hush| {
            let file = app.current_story().and_then(|s| s.file.clone());
            file.is_some_and(|f| matches!(app.files.look(&f), files::Look::Picture(Some(_))))
        };
        until(&hush, cx, decoded);

        // a message comes in the chat behind: its rows reload, the photo shown stays
        hush.update(cx, |app, cx| {
            let fewer = app.messages[1..].to_vec();
            app.set_messages(fewer, cx);
        });
        assert!(hush.read_with(cx, |app, _| decoded(app)));
        // both of Ana's seen: nothing is left asking the core as the test ends (one closing as the
        // test binary exits crashes it)
        until(&hush, cx, |app| {
            app.stories.list.iter().filter(|s| s.author == ANA).all(|s| s.viewed)
        });
        hush.update(cx, |app, _| app.client = None);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[gpui::test]
    fn a_story_that_expires_while_shown_gives_its_place_to_the_next(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        cx.simulate_keystrokes(window, "right");
        let (first, _) = shown(&hush, cx).expect("Ana's first");
        hush.update(cx, |app, cx| {
            let list = app.stories.list.iter().filter(|s| key(s) != first).cloned().collect();
            app.set_stories(list, cx);
        });
        let (now, _) = shown(&hush, cx).expect("her next");
        assert_eq!(now.0, ANA);
        assert_ne!(now, first);

        // all of hers went: nothing shows, as on the Mac
        hush.update(cx, |app, cx| {
            let list = app.stories.list.iter().filter(|s| s.author != ANA).cloned().collect();
            app.set_stories(list, cx);
        });
        assert_eq!(shown(&hush, cx), None);
        // the others are still listed, to pick
        assert!(hush.read_with(cx, |app, _| app.story_groups().len() == 3));
    }
}
