//! Typing indicators (the Mac app's `AppStore.typingUntil`, `TypingRow` and `TypingBubble`): who is
//! typing in each chat, shown at the end of its timeline, under its name in the toolbar and in its
//! row of the chat list; and our own, told to the core as the composer changes by hand.

use std::collections::{BTreeMap, HashMap};
use std::f32::consts::TAU;
use std::time::Duration;

use gpui_kit::component::h_flex;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind};

use super::view::avatar;
use super::Hush;
use crate::style;

/// What the toolbar and the chat list say while someone types.
pub(super) const WORDS: &str = "typing…";
/// Someone's typing that no stop ended goes after this long (`AppStore.typingTimeout`); a sender
/// repeats its start every 10 s while typing goes on.
const TIMEOUT: Duration = Duration::from_secs(15);
/// One wave of the dots, left to right.
const PULSE: Duration = Duration::from_millis(1250);

#[derive(Default)]
pub(super) struct Typing {
    /// Who is typing where (in a group, several may), each with the start that times them out; by
    /// ACI, as the Mac app picks the one shown.
    typers: HashMap<String, BTreeMap<String, u64>>,
    /// Counts the starts, so a timeout a newer start overtook does nothing.
    starts: u64,
    /// The chat the core was told we're typing in, until it's told we stopped.
    ours: Option<String>,
    /// What the core was told, in order.
    #[cfg(test)]
    told: Vec<(String, bool)>,
}

impl Typing {
    /// `who` started or stopped typing in `chat`: a start's number, for its timeout.
    fn set(&mut self, chat: &str, who: &str, started: bool) -> Option<u64> {
        let start = started.then(|| {
            self.starts += 1;
            self.starts
        });
        let typers = self.typers.entry(chat.to_owned()).or_default();
        match start {
            Some(n) => typers.insert(who.to_owned(), n),
            None => typers.remove(who),
        };
        if typers.is_empty() {
            self.typers.remove(chat);
        }
        start
    }

    /// Start `n`'s timeout: it ends that typing, unless a newer start or a stop came since. True if
    /// it did.
    fn expire(&mut self, chat: &str, who: &str, n: u64) -> bool {
        let current = self.typers.get(chat).and_then(|t| t.get(who)) == Some(&n);
        if current {
            self.set(chat, who, false);
        }
        current
    }

    /// The one shown typing in `chat` (the Mac app shows one).
    fn typer(&self, chat: &str) -> Option<&str> {
        self.typers.get(chat)?.keys().next().map(String::as_str)
    }
}

impl Hush {
    /// Someone started or stopped typing (`CoreEvent::Typing`); a start shows until a stop, or
    /// until it times out.
    pub(super) fn typing_event(&mut self, chat: String, who: String, started: bool, cx: &mut Context<Self>) {
        let start = self.typing.set(&chat, &who, started);
        self.typing_shown(&chat, cx);
        if let Some(n) = start {
            // a group's bubble says who
            if chat.starts_with("g:") {
                self.load_names(vec![who.clone()], cx);
            }
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TIMEOUT).await;
                let _ = this.update(cx, |app, cx| {
                    if app.typing.expire(&chat, &who, n) {
                        app.typing_shown(&chat, cx);
                    }
                });
            })
            .detach();
        }
    }

    /// The open chat's bubble may have come or gone: it rides on the last row.
    fn typing_shown(&mut self, chat: &str, cx: &mut Context<Self>) {
        if let (true, Some(last)) = (
            self.selected.as_deref() == Some(chat),
            self.messages.len().checked_sub(1),
        ) {
            self.timeline.update(cx, |t, cx| t.remeasure_items(last..last + 1, cx));
        }
        cx.notify();
    }

    /// Someone is typing in `chat`.
    pub(super) fn is_typing(&self, chat: &str) -> bool {
        self.typing.typer(chat).is_some()
    }

    /// The bubble at the end of chat `c`'s timeline while someone types there.
    /// shortcut: it rides on the last message's row, so a chat with no message loaded shows none;
    /// give the timeline a row of its own for it if typing in empty chats matters.
    pub(super) fn typing_bubble(&self, c: &Conversation) -> Option<Bubble> {
        let who = self.typing.typer(&c.id)?;
        Some(Bubble {
            typer: (c.kind == ConversationKind::Group).then(|| (format!("c:{who}"), self.name(who))),
        })
    }

    /// The composer changed by hand: the open chat is told we're typing, or that we stopped once
    /// nothing is left to send. Each change is told: the core repeats the start while they go on,
    /// and stops by itself 10 s after the last one.
    pub(super) fn composer_typed(&mut self) {
        match self.selected.clone() {
            Some(id) if !self.composer_empty => {
                self.typing.ours = Some(id.clone());
                self.tell_typing(id, true);
            }
            _ => self.stop_typing(),
        }
    }

    /// We stopped typing where we were: sent, or the text replaced (another chat's draft, an edit).
    pub(super) fn stop_typing(&mut self) {
        if let Some(id) = self.typing.ours.take() {
            self.tell_typing(id, false);
        }
    }

    fn tell_typing(&mut self, id: String, started: bool) {
        #[cfg(test)]
        self.typing.told.push((id.clone(), started));
        let Some(client) = &self.client else { return };
        // not shown: typing is a hint, and a message sent there says what's wrong
        if let Err(e) = client.set_typing(id, started) {
            tracing::warn!(error = %e, "typing not told");
        }
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s Family group with Bogdan typing in it, and Ana typing in hers.
    pub(super) fn demo_typing(&mut self, cx: &mut Context<Self>) {
        self.typing_event("g:01".into(), crate::demo::BOGDAN.into(), true, cx);
        let ana = crate::demo::ANA;
        self.typing_event(format!("c:{ana}"), ana.into(), true, cx);
    }
}

/// Someone typing, as the last thing in the timeline: an incoming bubble of three dots pulsing left
/// to right (still under Reduce Motion), after the typer's picture in a group (`TypingBubble`).
#[derive(Clone, IntoElement)]
pub(super) struct Bubble {
    /// Who, in a group: their chat's id and their name.
    typer: Option<(String, String)>,
}

impl RenderOnce for Bubble {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = style::palette(cx);
        let still = cx.reduce_motion();
        let dots = (0..3u8).map(|i| {
            let dot = div().size(px(7.)).rounded_full().bg(p.secondary);
            if still {
                return dot.opacity(0.6).into_any_element();
            }
            let wave = Animation::new(PULSE).repeat().with_max_fps(30.);
            dot.with_animation(("typing-dot", i as usize), wave, move |dot, t| {
                // each dot a little after the one before it
                let crest = (t * TAU - f32::from(i) * 0.7).sin().max(0.);
                dot.opacity(0.35 + 0.65 * crest)
            })
            .into_any_element()
        });
        let label = match &self.typer {
            Some((_, name)) => format!("{name} is typing"),
            None => "Typing".into(),
        };
        h_flex()
            .id("typing")
            .role(Role::Label)
            .aria_label(label)
            .w_full()
            .items_end()
            .gap(px(8.))
            .pt(px(6.))
            .pr(px(80.))
            .children(self.typer.map(|(id, name)| avatar(&id, &name, None, 28.)))
            .child(
                h_flex()
                    .gap(px(4.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(18.))
                    .bg(p.bubble_in)
                    .children(dots),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::Typing;

    #[test]
    fn typing_shows_until_it_stops_or_times_out() {
        let mut t = Typing::default();
        let first = t.set("c:a", "a", true).expect("a start is timed");
        assert_eq!(t.typer("c:a"), Some("a"));
        assert_eq!(t.typer("c:b"), None, "only where they type");
        assert!(t.set("c:a", "a", false).is_none());
        assert_eq!(t.typer("c:a"), None, "a stop ends it");
        assert!(!t.expire("c:a", "a", first), "already ended");

        let first = t.set("c:a", "a", true).unwrap();
        let again = t.set("c:a", "a", true).unwrap();
        assert!(!t.expire("c:a", "a", first), "a newer start overtook it");
        assert_eq!(t.typer("c:a"), Some("a"));
        assert!(t.expire("c:a", "a", again), "no stop came");
        assert_eq!(t.typer("c:a"), None);
    }

    #[test]
    fn in_a_group_one_typer_shows_while_any_types() {
        let mut t = Typing::default();
        let b = t.set("g:1", "b", true).unwrap();
        t.set("g:1", "a", true);
        assert_eq!(t.typer("g:1"), Some("a"), "the first by ACI, as on the Mac");
        t.set("g:1", "a", false);
        assert_eq!(t.typer("g:1"), Some("b"), "b still types");
        t.expire("g:1", "b", b);
        assert_eq!(t.typer("g:1"), None);
    }
}

/// Typing in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
    use sig_core::CoreEvent;

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use super::TIMEOUT;
    use crate::demo::{ANA, BOGDAN};
    use crate::Demo;

    /// The demo's Family group open, its composer focused.
    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        in_window(window, &hush, cx, |app, window, cx| {
            app.composer.update(cx, |c, cx| c.focus(window, cx))
        });
        cx.run_until_parked();
        (window, hush)
    }

    fn in_window(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut Window, &mut Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .unwrap();
    }

    fn typing(hush: &Entity<Hush>, chat: &str, who: &str, started: bool, cx: &mut TestAppContext) {
        hush.update(cx, |app, cx| {
            app.handle(
                CoreEvent::Typing {
                    conversation_id: chat.into(),
                    who: who.into(),
                    started,
                },
                cx,
            )
        });
        cx.run_until_parked();
    }

    fn typer(hush: &Entity<Hush>, chat: &str, cx: &mut TestAppContext) -> Option<String> {
        hush.read_with(cx, |app, _| app.typing.typer(chat).map(str::to_owned))
    }

    /// What the core was told since last asked.
    fn told(hush: &Entity<Hush>, cx: &mut TestAppContext) -> Vec<(String, bool)> {
        hush.update(cx, |app, _| std::mem::take(&mut app.typing.told))
    }

    #[gpui::test]
    fn someones_typing_shows_until_they_stop_or_for_15_seconds(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        // still dots: no frames drawn while the clock moves on
        cx.update(|cx| cx.set_reduce_motion(true));
        let ana = format!("c:{ANA}");
        typing(&hush, &ana, ANA, true, cx);
        assert_eq!(typer(&hush, &ana, cx).as_deref(), Some(ANA));
        typing(&hush, &ana, ANA, false, cx);
        assert_eq!(typer(&hush, &ana, cx), None, "stopped");

        // in the open group: its bubble names who
        typing(&hush, "g:01", BOGDAN, true, cx);
        let bubble = hush.read_with(cx, |app, _| {
            let family = app.chats.iter().find(|c| c.id == "g:01").unwrap();
            app.typing_bubble(family).and_then(|b| b.typer)
        });
        assert_eq!(bubble, Some((format!("c:{BOGDAN}"), "Bogdan Ionescu".into())));

        cx.executor().advance_clock(TIMEOUT / 2);
        // still typing: their start again times them out afresh
        typing(&hush, "g:01", BOGDAN, true, cx);
        cx.executor().advance_clock(TIMEOUT / 2);
        cx.run_until_parked();
        assert!(typer(&hush, "g:01", cx).is_some(), "the first timeout was overtaken");
        cx.executor().advance_clock(TIMEOUT / 2);
        cx.run_until_parked();
        assert_eq!(typer(&hush, "g:01", cx), None, "no stop came");
    }

    #[gpui::test]
    fn our_typing_is_told_while_we_write_and_stopped_when_we_send_clear_or_leave(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let started = |chat: &str| (chat.to_owned(), true);
        let stopped = |chat: &str| (chat.to_owned(), false);
        cx.simulate_input(window, "hi");
        assert_eq!(told(&hush, cx), [started("g:01"), started("g:01")], "every keystroke");

        cx.simulate_keystrokes(window, "secondary-a backspace");
        assert_eq!(told(&hush, cx), [stopped("g:01")], "cleared by hand");
        cx.simulate_input(window, " ");
        assert_eq!(told(&hush, cx), [], "nothing to send: not typing");

        // an edit's text, and the draft back after it, are put in, not typed
        cx.simulate_input(window, "x");
        in_window(window, &hush, cx, |app, window, cx| {
            let ours = app.messages.iter().rev().find(|m| m.is_outgoing && !m.deleted).cloned();
            app.start_editing(&ours.expect("the demo has one"), window, cx);
            app.cancel(window, cx);
        });
        assert_eq!(told(&hush, cx), [started("g:01"), stopped("g:01")]);

        // another chat: what's typed here waits as its draft
        cx.simulate_input(window, "y");
        let ana = format!("c:{ANA}");
        in_window(window, &hush, cx, |app, window, cx| app.select(ana.clone(), window, cx));
        assert_eq!(told(&hush, cx), [started("g:01"), stopped("g:01")]);

        // sent (not linked: it fails, and its text comes back without typing)
        let data = std::env::temp_dir().join(format!("hush-desktop-typing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        cx.simulate_input(window, "see you");
        told(&hush, cx);
        in_window(window, &hush, cx, |app, window, cx| app.send(window, cx));
        cx.run_until_parked();
        assert_eq!(told(&hush, cx), [stopped(&ana)]);
        hush.update(cx, |app, _| app.client = None);
        let _ = std::fs::remove_dir_all(&data);
    }
}
