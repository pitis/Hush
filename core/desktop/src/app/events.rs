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
    /// Reset Secure Session… under the notice: the chat, with whom, and whether a reset is on its
    /// way.
    reset: Option<(String, String, bool)>,
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
    /// loaded from its start), as its messages load.
    pub(super) fn reload_events(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (self.client.clone(), self.selected.clone()) else {
            return;
        };
        self.events.asked += 1;
        let (asked, generation) = (self.events.asked, self.generation);
        let since = match self.messages.first() {
            Some(m) if self.may_have_older() => m.sent_at,
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
        // the rows they show in grew or shrank
        self.timeline.update(cx, |t, cx| t.remeasure(cx));
    }

    /// An event was recorded in a chat: a reset, once it is, shows instead of its button.
    pub(super) fn event_added(&mut self, chat: &str, kind: ChatEventKind, cx: &mut Context<Self>) {
        if kind == ChatEventKind::SessionReset {
            self.events.resetting.remove(chat);
        }
        if self.selected.as_deref() == Some(chat) {
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
        if !loaded || (i == 0 && self.may_have_older()) {
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
                    let offer = (direct && !reset).then(|| {
                        let resetting = self.events.resetting.contains(&c.id);
                        (c.id.clone(), who.clone(), resetting)
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
        div().flex_1().min_h_0().child(
            v_flex()
                .id("events-only")
                .size_full()
                .overflow_y_scroll()
                .justify_end()
                .px(px(16.))
                .pb(px(8.))
                .children(render(&lines, c.wallpaper.is_some(), &this, p)),
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
        self.events = Events {
            generation: Some(self.generation),
            list,
            ..Default::default()
        };
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
        let reset = l.reset.clone().map(|(chat, who, resetting)| {
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

    /// Midday, so a few minutes either way stay on one day wherever the tests run.
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
    use gpui_kit::{gpui, AppContext as _, Entity, TestAppContext, WindowOptions};
    use sig_core::projection::ChatEventKind;
    use sig_core::CoreEvent;

    use super::super::Hush;
    use crate::Demo;

    fn open(cx: &mut TestAppContext) -> Entity<Hush> {
        let (_, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Events, window, cx))
            })
            .expect("the window opens")
        });
        cx.run_until_parked();
        hush
    }

    /// Whether the newest notice of messages that couldn't be decrypted says its reset is on its
    /// way.
    fn resetting(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, _| {
            let c = app.chats.iter().find(|c| Some(&c.id) == app.selected.as_ref()).unwrap();
            let after = app.event_lines(c, app.messages.len(), chrono::Local::now());
            let notice = after.last().and_then(|l| l.reset.clone()).expect("it offers a reset");
            notice.2
        })
    }

    #[gpui::test]
    fn a_reset_on_its_way_shows_until_it_is_recorded_or_anything_fails(cx: &mut TestAppContext) {
        let hush = open(cx);
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
}
