//! What happened in the open chat that isn't a message, among its messages: timer changes, calls,
//! session resets, safety numbers, and messages that couldn't be decrypted, with Reset Secure
//! Session… (the Mac app's `ChatEventRow`, `DecryptionNotice` and `Timeline.items`). A message's row
//! shows the events before it, and the last row those after it too, so the timeline's rows stay its
//! messages.

use std::collections::HashSet;
use std::ops::Range;

use async_compat::Compat;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{ChatEvent, ChatEventKind, Conversation, ConversationKind, Message};

use super::options::backed;
use super::view::{day_line, icon, label};
use super::{describe, Hush};
use crate::style::Palette;
use crate::text;

/// The open chat's events, and the secure-session resets on their way.
#[derive(Default)]
pub(super) struct Events {
    /// The chat's `generation` they were loaded for: another chat's never show.
    generation: Option<u64>,
    /// Its events over the span of its loaded messages, oldest first.
    list: Vec<ChatEvent>,
    /// Bumped on every load, so an answer a newer load overtook is dropped.
    asked: u64,
    /// The messages shown go back to the chat's start, as of the last load: the events before
    /// them show too.
    from_start: bool,
    /// Where a chat with events only is scrolled to.
    scroll: ScrollHandle,
    /// Chats whose secure-session reset is on its way: until it's recorded, or anything fails.
    resetting: HashSet<String>,
}

/// An event, or a notice of messages that couldn't be decrypted, as the timeline shows it.
pub(super) struct Line {
    /// Its (first) event's.
    id: i64,
    /// A day separator above it ("Today").
    day: Option<String>,
    symbol: &'static str,
    text: String,
    /// A missed call: in red.
    missed: bool,
    /// Reset Secure Session… under the notice.
    reset: Option<Reset>,
}

/// Reset Secure Session… for a chat (the one clicked, whatever is open when the prompt is answered).
#[derive(Clone)]
struct Reset {
    chat: String,
    /// Whom the prompt names.
    who: String,
    /// One is on its way.
    resetting: bool,
}

/// What a stretch of events shows, in order: each event, but messages that couldn't be decrypted one
/// after another on one day, which show as one notice (`Timeline.Item.failures`).
#[derive(Debug, PartialEq)]
enum Shown {
    Event(usize),
    /// The first of them, how many, and whether the secure session was reset after them.
    Failures {
        first: usize,
        count: usize,
        reset: bool,
    },
}

impl Hush {
    /// Reloads the open chat's events since its oldest loaded message (all of them, with the chat
    /// loaded from its start), as its messages are set (`depth` is theirs then).
    pub(super) fn reload_events(&mut self, cx: &mut Context<Self>) {
        self.events.from_start = !self.may_have_older();
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        self.events.asked += 1;
        let (asked, generation) = (self.events.asked, self.generation);
        let since = match self.messages.first() {
            Some(m) if !self.events.from_start => m.sent_at,
            _ => 0,
        };
        cx.spawn(async move |this, cx| {
            let found = Compat::new(client.chat_events(id, since)).await;
            let _ = this.update(cx, |app, cx| {
                if app.events.asked != asked || app.generation != generation {
                    return;
                }
                match found {
                    Ok(list) => app.set_events(list, cx),
                    Err(e) => app.error = Some(format!("Couldn't load the chat's events: {}", describe(&e))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_events(&mut self, list: Vec<ChatEvent>, cx: &mut Context<Self>) {
        if self.events.generation == Some(self.generation) && self.events.list == list {
            return;
        }
        // in a group, whom each is about
        self.load_names(list.iter().map(|e| e.author.clone()).collect(), cx);
        self.events.generation = Some(self.generation);
        self.events.list = list;
        // a chat with events only opens at its newest, as the timeline does
        self.events.scroll.scroll_to_bottom();
        // the rows they show in grew or shrank
        self.timeline.update(cx, |t, cx| t.remeasure(cx));
    }

    /// An event was recorded in a chat: a reset, once it is, shows instead of its button.
    pub(super) fn event_added(&mut self, chat: &str, kind: ChatEventKind, cx: &mut Context<Self>) {
        if kind == ChatEventKind::SessionReset {
            self.events.resetting.remove(chat);
        }
        // a load on its way reloads them once its messages are in
        let loading = self.messages_loading == Some(self.generation);
        if self.selected.as_deref() == Some(chat) && !loading {
            self.reload_events(cx);
        }
    }

    /// Something failed, maybe a reset: it is offered again (a second one does no harm).
    pub(super) fn resets_failed(&mut self) {
        self.events.resetting.clear();
    }

    /// The open chat's events between the message before `i` and message `i` (`messages.len()`:
    /// after the last). Those before the oldest message loaded wait for the chat to be loaded from
    /// its start: older messages may come between.
    fn gap_at(&self, i: usize) -> Range<usize> {
        let loaded = self.events.generation == Some(self.generation);
        if !loaded || (i == 0 && !self.events.from_start) {
            return 0..0;
        }
        gap(&self.events.list, &self.messages, i)
    }

    /// The events the timeline shows between the message before `i` and message `i`.
    pub(super) fn events_before(&self, i: usize) -> &[ChatEvent] {
        &self.events.list[self.gap_at(i)]
    }

    /// The lines the events before message `i` of chat `c` show (`messages.len()`: after the last).
    pub(super) fn event_lines(&self, c: &Conversation, i: usize, now: chrono::DateTime<chrono::Local>) -> Vec<Line> {
        let list = &self.events.list;
        // presage resets sessions by ACI only
        let direct = c.id.starts_with("c:") && !c.id.starts_with("c:PNI:");
        let mut above = i.checked_sub(1).map(|j| self.messages[j].sent_at);
        let mut lines = vec![];
        for shown in shown(list, self.gap_at(i)) {
            let (first, last) = match shown {
                Shown::Event(j) => (j, j),
                Shown::Failures { first, count, .. } => (first, first + count - 1),
            };
            let e = &list[first];
            // a direct chat's events are about it, a group's about their authors
            let who = match c.kind {
                ConversationKind::Group => self.name(&e.author),
                ConversationKind::Direct => text::title(c, self.own_aci.as_deref()),
            };
            let (symbol, words, reset) = match shown {
                Shown::Event(_) => {
                    let (symbol, words) = said(e, &who, self.own_aci.as_deref() == Some(e.author.as_str()));
                    (symbol, words, None)
                }
                Shown::Failures { count, reset, .. } => {
                    let offer = (direct && !reset).then(|| Reset {
                        chat: c.id.clone(),
                        who: who.clone(),
                        resetting: self.events.resetting.contains(&c.id),
                    });
                    ("shield-alert", failures(count, &who), offer)
                }
            };
            lines.push(Line {
                id: e.id,
                day: above
                    .is_none_or(|at| !text::same_day(at, e.at))
                    .then(|| text::day(e.at, now)),
                symbol,
                text: words,
                missed: e.kind == ChatEventKind::MissedCall,
                reset,
            });
            above = Some(list[last].at);
        }
        lines
    }

    /// The timeline; a chat with no messages loaded shows its events alone instead (the timeline's
    /// rows are its messages').
    pub(super) fn with_events(&self, timeline: Div, c: &Conversation, p: Palette, cx: &mut Context<Self>) -> Div {
        if !self.messages.is_empty() {
            return timeline;
        }
        let lines = self.event_lines(c, 0, chrono::Local::now());
        if lines.is_empty() {
            return timeline;
        }
        let this = cx.entity().downgrade();
        // at the bottom while they fit, scrolled from the top when they don't
        div().flex_1().min_h_0().child(
            div()
                .id("events-only")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.events.scroll)
                .child(
                    v_flex()
                        .min_h_full()
                        .justify_end()
                        .px(px(16.))
                        .pb(px(8.))
                        .children(render(&lines, c.wallpaper.is_some(), &this, p)),
                ),
        )
    }

    /// Asks first, as the Mac app's confirmation, then starts a new secure session with them.
    fn confirm_reset(&mut self, chat: String, who: String, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "Hush starts a new encrypted session. {who} will see that it was reset. Messages that \
             couldn't be decrypted stay unreadable: ask them to resend anything important."
        );
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Reset the secure session with {who}?"),
            Some(&detail),
            &["Reset", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |app, cx| app.reset_session(chat, cx));
            }
        })
        .detach();
    }

    fn reset_session(&mut self, chat: String, cx: &mut Context<Self>) {
        let Some(client) = &self.client else { return };
        match client.reset_session(chat.clone()) {
            Ok(()) => {
                self.events.resetting.insert(chat);
            }
            Err(e) => self.error = Some(format!("Couldn't reset the session: {}", describe(&e))),
        }
        cx.notify();
    }
}

#[cfg(feature = "snapshot")]
impl Hush {
    /// `--snapshot`'s chat with disappearing messages on, and events of each kind among its
    /// messages.
    pub(super) fn demo_events(&mut self, cx: &mut Context<Self>) {
        let id = format!("c:{}", crate::demo::ANA);
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            c.expire_timer = 604_800;
        }
        self.selected = Some(id);
        let (messages, list) = crate::demo::timed_chat();
        self.set_events(list, cx);
        self.set_messages(messages, cx);
    }
}

/// The events' lines, centred between the messages, each under its day separator; on a wallpaper,
/// on a backing (`ChatEventRow`, `DecryptionNotice`).
pub(super) fn render<'a>(
    lines: &'a [Line],
    wallpaper: bool,
    this: &'a WeakEntity<Hush>,
    p: Palette,
) -> impl Iterator<Item = Stateful<Div>> + 'a {
    lines.iter().map(move |l| {
        let color = if l.missed { p.danger.opacity(0.9) } else { p.secondary };
        let reset = l.reset.clone().map(|Reset { chat, who, resetting }| {
            let this = this.clone();
            let words = if resetting {
                "Resetting…"
            } else {
                "Reset Secure Session…"
            };
            div()
                .id("reset-session")
                .role(Role::Button)
                .aria_label(words)
                .text_size(px(12.))
                .text_color(if resetting { p.secondary } else { p.accent })
                .child(words)
                .when(!resetting, |button| {
                    button.cursor_pointer().on_click(move |_, window, cx| {
                        let _ = this.update(cx, |app, cx| app.confirm_reset(chat.clone(), who.clone(), window, cx));
                    })
                })
        });
        let event = v_flex()
            .items_center()
            .gap(px(4.))
            .child(
                h_flex().gap(px(4.)).child(icon(l.symbol, 12., color)).child(
                    label("event-text", Role::Label, l.text.clone())
                        .text_size(px(12.))
                        .text_color(color),
                ),
            )
            .children(reset);
        v_flex()
            .id(("event", l.id as usize))
            .w_full()
            .items_center()
            .children(l.day.clone().map(|day| day_line(day, wallpaper, p)))
            .child(div().py(px(8.)).child(backed(event, wallpaper, p)))
    })
}

/// The events between the message before `i` and message `i` (`messages.len()`: after the last),
/// as indexes into `events`; at the same time, the message comes first (`Timeline.items`).
fn gap(events: &[ChatEvent], messages: &[Message], i: usize) -> Range<usize> {
    let before = |m: &Message| events.partition_point(|e| e.at < m.sent_at);
    let start = i.checked_sub(1).and_then(|j| messages.get(j)).map_or(0, before);
    let end = messages.get(i).map_or(events.len(), before);
    start..end
}

/// What the events in `gap` show.
fn shown(events: &[ChatEvent], gap: Range<usize>) -> Vec<Shown> {
    let mut out: Vec<Shown> = vec![];
    for j in gap {
        let e = &events[j];
        if e.kind != ChatEventKind::DecryptionFailed {
            out.push(Shown::Event(j));
            continue;
        }
        // right after another failure, on its day
        if let Some(Shown::Failures { first, count, .. }) = out.last_mut() {
            if text::same_day(events[*first + *count - 1].at, e.at) {
                *count += 1;
                continue;
            }
        }
        let reset = events[j + 1..].iter().any(|e| e.kind == ChatEventKind::SessionReset);
        out.push(Shown::Failures {
            first: j,
            count: 1,
            reset,
        });
    }
    out
}

/// An event's symbol and words; `who` is whom it's about (in a group its author, else the chat),
/// `ours` that we did it.
fn said(e: &ChatEvent, who: &str, ours: bool) -> (&'static str, String) {
    let call = |what: &str| match e.value {
        Some(s) if s > 0 => format!("{what} ({}:{:02})", s / 60, s % 60),
        _ => what.to_owned(),
    };
    match e.kind {
        ChatEventKind::TimerChanged => {
            let subject = if ours { "You" } else { who };
            let words = match e.value.unwrap_or(0) {
                0 => format!("{subject} disabled disappearing messages."),
                s => format!(
                    "{subject} set the disappearing message time to {}.",
                    text::duration(u32::try_from(s).unwrap_or(u32::MAX))
                ),
            };
            ("hourglass", words)
        }
        ChatEventKind::SessionReset => ("rotate-ccw-key", format!("You reset the secure session with {who}")),
        ChatEventKind::OutgoingCall => ("phone-outgoing", call("Outgoing voice call")),
        ChatEventKind::IncomingCall => ("phone-incoming", call("Incoming voice call")),
        ChatEventKind::MissedCall => ("phone-missed", "Missed voice call".into()),
        ChatEventKind::SafetyNumberChanged => ("shield-check", format!("Your safety number with {who} changed")),
        // shown as `failures`
        ChatEventKind::DecryptionFailed => ("shield-alert", failures(1, who)),
    }
}

/// Their failures (retry receipts) show as ours do: worded for both.
fn failures(count: usize, who: &str) -> String {
    match count {
        1 => format!("A message between you and {who} couldn't be decrypted"),
        n => format!("{n} messages between you and {who} couldn't be decrypted"),
    }
}

#[cfg(test)]
mod tests {
    use sig_core::projection::{ChatEvent, ChatEventKind, Message, MessageKind, MessageStatus};

    use super::{failures, gap, said, shown, Shown};

    /// Noon UTC, at the earliest local midnight: the minutes after it stay on one day wherever the
    /// tests run.
    const NOON: i64 = 1_800_000_000_000 - 1_800_000_000_000 % 86_400_000 + 12 * 3_600_000;
    const MINUTE: i64 = 60_000;

    fn event(id: i64, at: i64, kind: ChatEventKind) -> ChatEvent {
        ChatEvent {
            id,
            conversation_id: "c:ana".into(),
            at,
            kind,
            author: "ana".into(),
            value: None,
        }
    }

    fn message(sent_at: i64) -> Message {
        Message {
            conversation_id: "c:ana".into(),
            author: "ana".into(),
            sent_at,
            received_at: sent_at,
            body: Some("hello".into()),
            ranges: vec![],
            is_outgoing: false,
            status: MessageStatus::Read,
            edited: false,
            deleted: false,
            quote: None,
            kind: MessageKind::Normal,
            expires_in: 0,
            expires_at: None,
            story: None,
        }
    }

    #[test]
    fn events_go_between_the_messages_by_time() {
        use ChatEventKind::*;
        let messages = [message(NOON), message(NOON + 10 * MINUTE)];
        let events = [
            event(1, NOON - MINUTE, MissedCall),
            // at a message's time: after it
            event(2, NOON, TimerChanged),
            event(3, NOON + 5 * MINUTE, OutgoingCall),
            event(4, NOON + 20 * MINUTE, SessionReset),
        ];
        assert_eq!(gap(&events, &messages, 0), 0..1);
        assert_eq!(gap(&events, &messages, 1), 1..3);
        assert_eq!(gap(&events, &messages, 2), 3..4, "after the last message");
        assert_eq!(gap(&events, &[], 0), 0..4, "a chat with events only");
        assert_eq!(gap(&[], &messages, 1), 0..0);
    }

    #[test]
    fn failures_in_a_row_show_as_one_notice_answered_by_a_later_reset() {
        use ChatEventKind::*;
        let events = [
            event(1, NOON, DecryptionFailed),
            event(2, NOON + MINUTE, DecryptionFailed),
            event(3, NOON + 2 * MINUTE, DecryptionFailed),
            event(4, NOON + 3 * MINUTE, MissedCall),
            event(5, NOON + 4 * MINUTE, DecryptionFailed),
            // another day
            event(6, NOON + 48 * 60 * MINUTE, DecryptionFailed),
            event(7, NOON + 48 * 60 * MINUTE + MINUTE, SessionReset),
            event(8, NOON + 48 * 60 * MINUTE + 2 * MINUTE, DecryptionFailed),
        ];
        assert_eq!(
            shown(&events, 0..events.len()),
            [
                Shown::Failures {
                    first: 0,
                    count: 3,
                    reset: true
                },
                Shown::Event(3),
                Shown::Failures {
                    first: 4,
                    count: 1,
                    reset: true
                },
                Shown::Failures {
                    first: 5,
                    count: 1,
                    reset: true
                },
                Shown::Event(6),
                Shown::Failures {
                    first: 7,
                    count: 1,
                    reset: false
                },
            ]
        );
        // a message between them: two notices, each in its own gap
        assert_eq!(
            shown(&events, 0..1),
            [Shown::Failures {
                first: 0,
                count: 1,
                reset: true
            }]
        );
        assert_eq!(
            shown(&events, 1..2),
            [Shown::Failures {
                first: 1,
                count: 1,
                reset: true
            }]
        );
    }

    #[test]
    fn events_are_worded_as_the_mac_app_words_them() {
        use ChatEventKind::*;
        let with = |kind, value| ChatEvent {
            value,
            ..event(1, NOON, kind)
        };
        let words = |e: ChatEvent, ours| said(&e, "Ana", ours).1;
        assert_eq!(
            words(with(TimerChanged, Some(3_600)), false),
            "Ana set the disappearing message time to 1 hour."
        );
        assert_eq!(
            words(with(TimerChanged, Some(0)), true),
            "You disabled disappearing messages."
        );
        assert_eq!(
            words(with(SessionReset, None), true),
            "You reset the secure session with Ana"
        );
        assert_eq!(words(with(OutgoingCall, Some(125)), true), "Outgoing voice call (2:05)");
        assert_eq!(
            words(with(OutgoingCall, None), true),
            "Outgoing voice call",
            "never answered"
        );
        assert_eq!(words(with(IncomingCall, Some(9)), false), "Incoming voice call (0:09)");
        assert_eq!(words(with(MissedCall, None), false), "Missed voice call");
        assert_eq!(
            words(with(SafetyNumberChanged, None), false),
            "Your safety number with Ana changed"
        );
        assert_eq!(said(&with(TimerChanged, Some(30)), "Ana", false).0, "hourglass");
        assert_eq!(
            failures(1, "Ana"),
            "A message between you and Ana couldn't be decrypted"
        );
        assert_eq!(
            failures(3, "Ana"),
            "3 messages between you and Ana couldn't be decrypted"
        );
    }
}

/// The events in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowOptions};
    use sig_core::projection::{ChatEvent, ChatEventKind, Conversation, ConversationKind};
    use sig_core::CoreEvent;

    use super::super::attach::window_tests::with_core;
    use super::super::{Hush, PAGE};
    use super::Line;
    use crate::demo::{ANA, ME};
    use crate::Demo;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Events, window, cx))
            })
            .expect("the window opens")
        });
        cx.run_until_parked();
        opened
    }

    fn in_window(
        window: AnyWindowHandle,
        hush: &Entity<Hush>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Hush, &mut gpui_kit::Window, &mut gpui_kit::Context<Hush>),
    ) {
        window
            .update(cx, |_, window, cx| hush.update(cx, |app, cx| f(app, window, cx)))
            .unwrap();
    }

    fn open_chat(app: &Hush) -> Conversation {
        let open = app.chats.iter().find(|c| Some(&c.id) == app.selected.as_ref());
        open.expect("a chat is open").clone()
    }

    /// The lines after the last message, of the open chat or of `c`.
    fn after(hush: &Entity<Hush>, c: Option<Conversation>, cx: &mut TestAppContext) -> Vec<Line> {
        hush.read_with(cx, |app, _| {
            let c = c.unwrap_or_else(|| open_chat(app));
            app.event_lines(&c, app.messages.len(), chrono::Local::now())
        })
    }

    /// Whether the newest notice of messages that couldn't be decrypted says its reset is on its
    /// way.
    fn resetting(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        let lines = after(hush, None, cx);
        lines
            .last()
            .and_then(|l| l.reset.clone())
            .expect("it offers a reset")
            .resetting
    }

    #[gpui::test]
    fn a_reset_on_its_way_shows_until_it_is_recorded_or_anything_fails(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let chat = hush.read_with(cx, |app, _| app.selected.clone().unwrap());
        assert!(!resetting(&hush, cx));
        let start = |cx: &mut TestAppContext| {
            hush.update(cx, |app, _| {
                app.events.resetting.insert(chat.clone());
            })
        };
        start(cx);
        assert!(resetting(&hush, cx));
        // another chat's reset, or another event here, isn't it
        let added = |chat: &str, kind| CoreEvent::ChatEventAdded {
            conversation_id: chat.into(),
            kind,
        };
        hush.update(cx, |app, cx| {
            app.handle(added("c:other", ChatEventKind::SessionReset), cx);
            app.handle(added(&chat, ChatEventKind::MissedCall), cx);
        });
        assert!(resetting(&hush, cx));
        hush.update(cx, |app, cx| app.handle(added(&chat, ChatEventKind::SessionReset), cx));
        assert!(!resetting(&hush, cx), "recorded");
        start(cx);
        let failed = CoreEvent::Error {
            message: "send failed".into(),
        };
        hush.update(cx, |app, cx| app.handle(failed, cx));
        assert!(!resetting(&hush, cx), "offered again after a failure");
    }

    #[gpui::test]
    fn the_reset_asks_first_and_one_the_core_refuses_is_said(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let data = std::env::temp_dir().join(format!("hush-desktop-events-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        with_core(&hush, &data, cx);
        let chat = format!("c:{ANA}");
        let ask = |cx: &mut TestAppContext| {
            let chat = chat.clone();
            in_window(window, &hush, cx, |app, window, cx| {
                app.confirm_reset(chat, "Ana".into(), window, cx)
            });
            cx.run_until_parked();
        };
        ask(cx);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!resetting(&hush, cx), "cancelled: nothing is sent");
        ask(cx);
        cx.simulate_prompt_answer("Reset");
        cx.run_until_parked();
        assert!(resetting(&hush, cx));
        // the core resets 1:1 sessions only
        hush.update(cx, |app, cx| app.reset_session("g:01".into(), cx));
        let error = hush.read_with(cx, |app, _| app.error.clone()).unwrap_or_default();
        assert!(error.starts_with("Couldn't reset the session: "), "{error}");
    }

    #[gpui::test]
    fn another_chats_events_never_show(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        // no core: the events loaded for Ana's chat stay, as when another chat's load is slow
        in_window(window, &hush, cx, |app, window, cx| {
            app.switch(Some("g:01".into()), window, cx);
            app.set_messages(crate::demo::messages(), cx);
        });
        hush.read_with(cx, |app, _| {
            let family = open_chat(app);
            for i in 0..=app.messages.len() {
                assert!(app.events_before(i).is_empty(), "row {i}");
                assert!(app.event_lines(&family, i, chrono::Local::now()).is_empty(), "row {i}");
            }
        });
    }

    #[gpui::test]
    fn events_before_the_oldest_message_wait_while_older_ones_may_come_between(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let before_first = |cx: &mut TestAppContext| hush.read_with(cx, |app, _| app.events_before(0).len());
        assert_eq!(before_first(cx), 3, "the whole chat is loaded: a day back's show");
        // a full page: older messages may come before these
        hush.update(cx, |app, cx| {
            app.depth = app.messages.len() as u32;
            app.set_messages(app.messages.clone(), cx);
        });
        assert_eq!(before_first(cx), 0);
        // and while an older page is on its way, with an event come meanwhile
        let chat = format!("c:{ANA}");
        hush.update(cx, |app, cx| {
            app.depth += PAGE;
            app.messages_loading = Some(app.generation);
            let added = CoreEvent::ChatEventAdded {
                conversation_id: chat,
                kind: ChatEventKind::MissedCall,
            };
            app.handle(added, cx);
        });
        assert_eq!(before_first(cx), 0, "not until the page is in");
    }

    #[gpui::test]
    fn events_name_whom_they_are_about_and_offer_a_reset_in_direct_chats_only(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let last = hush.read_with(cx, |app, _| app.messages.last().unwrap().sent_at);
        let event = |id: i64, at: i64, kind, author: &str| ChatEvent {
            id,
            conversation_id: format!("c:{ANA}"),
            at,
            kind,
            author: author.into(),
            value: Some(30),
        };
        const DAYS_2: i64 = 2 * 86_400_000;
        hush.update(cx, |app, _| {
            app.events.list = vec![
                event(1, last + 1, ChatEventKind::TimerChanged, ME),
                event(2, last + 2, ChatEventKind::DecryptionFailed, ANA),
                event(3, last + DAYS_2, ChatEventKind::MissedCall, ANA),
                event(4, last + DAYS_2 + 1, ChatEventKind::MissedCall, ANA),
            ];
        });
        let lines = after(&hush, None, cx);
        let days: Vec<bool> = lines.iter().map(|l| l.day.is_some()).collect();
        assert_eq!(days, [false, false, true, false], "a separator above a new day only");
        assert_eq!(lines[0].text, "You set the disappearing message time to 30 seconds.");
        let reset = lines[1].reset.clone().expect("a direct chat's notice offers a reset");
        assert_eq!((reset.chat, reset.who), (format!("c:{ANA}"), "Ana Popescu".into()));
        // a group names who it's about; neither it nor a phone number's chat can be reset here
        let ana = hush.read_with(cx, |app, _| open_chat(app));
        let group = Conversation {
            id: "g:x".into(),
            kind: ConversationKind::Group,
            title: "Family".into(),
            ..ana.clone()
        };
        let lines = after(&hush, Some(group), cx);
        assert!(lines[1].reset.is_none());
        assert_eq!(
            lines[1].text,
            "A message between you and Ana Popescu couldn't be decrypted"
        );
        let by_number = Conversation {
            id: "c:PNI:x".into(),
            ..ana
        };
        assert!(after(&hush, Some(by_number), cx)[1].reset.is_none());
    }
}
