//! Older messages loaded as the timeline is scrolled up, and the "New messages" pill for those that
//! arrive meanwhile (the Mac app's `TimelineList` and `AppStore.loadOlder`).

use gpui_kit::component::button::ButtonVariants;
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::Sizable;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Hush, PAGE};
use crate::style::Palette;

/// Scrolled up to within this many rows of the oldest one loaded, the page before it loads.
const NEAR_TOP: usize = 10;

/// What the open chat's timeline is waiting for; each is the chat's `generation`, so another
/// chat's never shows.
#[derive(Default)]
pub(super) struct Paging {
    /// An older page is on its way: a spinner at the top.
    older: Option<u64>,
    /// Messages came in while scrolled up: the "New messages" pill.
    unseen: Option<u64>,
}

impl Hush {
    /// The timeline scrolled or changed: back at the bottom, the pill goes; near the top, the page
    /// before the oldest loaded message loads (the newest messages load this much deeper).
    pub(super) fn timeline_moved(&mut self, cx: &mut Context<Self>) {
        let t = self.timeline.read(cx);
        if !t.is_scrolled_up() {
            if self.paging.unseen.take().is_some() {
                cx.notify();
            }
            return;
        }
        let near_top = t.logical_scroll_top().item_ix < NEAR_TOP;
        // fewer than asked for is the whole chat; fewer than wanted, a load still on its way
        let more = !self.messages.is_empty() && self.messages.len() as u32 >= self.depth;
        // one on its way (back to the newest, say) would move the reader once in: it comes first
        let loading = self.messages_loading == Some(self.generation);
        if near_top && more && !loading {
            self.paging.older = Some(self.generation);
            self.depth += PAGE;
            self.reload_messages(cx);
            cx.notify();
        }
    }

    /// The open chat's messages loaded (or didn't): the older page asked for is in.
    pub(super) fn older_loaded(&mut self) {
        self.paging.older = None;
    }

    /// The rows went from `old` to `new`: a message that came in at the end, while scrolled up,
    /// scrolls to it if it is ours, else shows the pill; at the bottom the timeline follows it
    /// (`Timeline.onNewLastMessage`).
    pub(super) fn new_rows<K: PartialEq>(&mut self, old: &[K], new: &[K], cx: &mut Context<Self>) {
        let came = !old.is_empty() && new.last().is_some_and(|k| !old.contains(k));
        if !came || !self.timeline.read(cx).is_scrolled_up() {
            return;
        }
        if self.messages.last().is_some_and(|m| m.is_outgoing) {
            self.timeline.update(cx, |t, cx| t.scroll_to_end(cx));
        } else {
            self.paging.unseen = Some(self.generation);
        }
    }

    /// The timeline, with a spinner over its top while an older page loads, and its jump button
    /// only as the Mac app's "New messages" pill, bottom right, while messages came in unseen.
    pub(super) fn with_paging(&self, timeline: MessageScroller, p: Palette) -> Div {
        let unseen = self.paging.unseen == Some(self.generation);
        let loading = self.paging.older == Some(self.generation);
        let timeline = timeline
            .jump_button(unseen)
            .with_jump_button_label("New messages")
            .with_jump_button_renderer(move |b| {
                b.primary()
                    .label("New messages")
                    .bg(p.accent)
                    .text_color(p.on_accent)
                    .border_color(p.accent)
                    // its row centres it
                    .ml_auto()
                    .mr(px(16.))
            });
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(timeline)
            .when(loading, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(8.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .id("loading-older")
                                .role(Role::ProgressIndicator)
                                .aria_label("Loading older messages")
                                .child(Spinner::new().small()),
                        ),
                )
            })
    }

    /// `--snapshot`'s history screen: the chat scrolled back, an older page loading and a message
    /// come in unseen.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo_paging(&mut self, cx: &mut Context<Self>) {
        let mut all = crate::demo::history(60);
        all.extend(crate::demo::messages());
        self.set_messages(all, cx);
        self.timeline.update(cx, |t, cx| t.scroll_to_item(20, cx));
        // after the timeline's own notice of the scroll, which comes before it is laid out
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            let shown = this.update(cx, |app, _| {
                app.paging.older = Some(app.generation);
                app.paging.unseen = Some(app.generation);
            });
            shown.expect("the demo window is open");
        });
    }
}

/// The timeline in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use std::time::{Duration, Instant};

    use gpui_kit::{gpui, AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowOptions};
    use sig_core::projection::{Message, MessageKey};
    use sig_core::CoreEvent;

    use super::super::{Hush, PAGE};
    use crate::Demo;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Hush>) {
        let opened = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::key_bindings());
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        cx.run_until_parked();
        opened
    }

    /// Lays the window out and paints it, as a frame on screen would.
    fn draw(window: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            })
            .expect("the window is open");
        cx.run_until_parked();
    }

    fn show(hush: &Entity<Hush>, messages: &[Message], cx: &mut TestAppContext) {
        hush.update(cx, |app, cx| app.set_messages(messages.to_vec(), cx));
    }

    fn scroll_to(hush: &Entity<Hush>, ix: usize, cx: &mut TestAppContext) {
        hush.update(cx, |app, cx| app.timeline.update(cx, |t, cx| t.scroll_to_item(ix, cx)));
    }

    /// The message at the top of the timeline.
    fn top(hush: &Entity<Hush>, cx: &mut TestAppContext) -> MessageKey {
        hush.read_with(cx, |app, cx| {
            let ix = app.timeline.read(cx).logical_scroll_top().item_ix;
            app.messages[ix].key()
        })
    }

    /// The pill shows, and the spinner.
    fn pill(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, _| app.paging.unseen == Some(app.generation))
    }

    fn loading_older(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, _| app.paging.older == Some(app.generation))
    }

    fn depth(hush: &Entity<Hush>, cx: &mut TestAppContext) -> u32 {
        hush.read_with(cx, |app, _| app.depth)
    }

    fn scrolled_up(hush: &Entity<Hush>, cx: &mut TestAppContext) -> bool {
        hush.read_with(cx, |app, cx| app.timeline.read(cx).is_scrolled_up())
    }

    // history(201): every third message is ours, from the first; 199 is Bogdan's, 200 ours
    #[gpui::test]
    fn a_reader_scrolled_up_stays_on_their_message_as_rows_come_and_go(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let all = crate::demo::history(201);
        show(&hush, &all[99..199], cx);
        draw(window, cx);
        scroll_to(&hush, 40, cx);
        draw(window, cx);
        assert_eq!(top(&hush, cx), all[139].key());

        // Bogdan's new message: the newest 100 load, so the oldest goes
        show(&hush, &all[100..200], cx);
        draw(window, cx);
        assert_eq!(top(&hush, cx), all[139].key(), "a new message at the end");
        assert!(pill(&hush, cx), "it shows as new");

        // an older page above
        show(&hush, &all[..200], cx);
        draw(window, cx);
        assert_eq!(top(&hush, cx), all[139].key(), "older messages above");
        assert!(pill(&hush, cx), "the new one is still unseen");

        // the pill's button: to the end, and it goes
        hush.update(cx, |app, cx| app.timeline.update(cx, |t, cx| t.scroll_to_end(cx)));
        draw(window, cx);
        assert!(!scrolled_up(&hush, cx));
        assert!(!pill(&hush, cx));
    }

    #[gpui::test]
    fn our_own_message_scrolls_to_the_end_and_another_chat_has_no_pill(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let all = crate::demo::history(201);
        show(&hush, &all[100..200], cx);
        draw(window, cx);
        scroll_to(&hush, 40, cx);
        draw(window, cx);
        show(&hush, &all[101..201], cx);
        draw(window, cx);
        assert!(!scrolled_up(&hush, cx), "to our message");
        assert!(!pill(&hush, cx));

        // at the end, theirs is followed: no pill either
        let mut theirs = all[200].clone();
        theirs.author = crate::demo::BOGDAN.into();
        theirs.is_outgoing = false;
        let mut now = all[101..201].to_vec();
        now.push(theirs);
        show(&hush, &now, cx);
        draw(window, cx);
        assert!(!scrolled_up(&hush, cx));
        assert!(!pill(&hush, cx));

        // the pill belongs to the chat it showed in
        scroll_to(&hush, 40, cx);
        draw(window, cx);
        show(&hush, &all[..], cx);
        hush.update(cx, |app, _| app.paging.unseen = Some(app.generation));
        window
            .update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.select("g:02".into(), window, cx))
            })
            .unwrap();
        assert!(!pill(&hush, cx));
    }

    #[gpui::test]
    fn scrolled_up_near_the_oldest_message_the_page_before_loads(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let all = crate::demo::history(300);
        show(&hush, &all[200..], cx);
        draw(window, cx);
        scroll_to(&hush, 30, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), PAGE, "not near the top yet");

        scroll_to(&hush, 5, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), 2 * PAGE);
        assert!(loading_older(&hush, cx), "the spinner shows");
        // more scrolling while it loads asks no more
        scroll_to(&hush, 2, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), 2 * PAGE);

        // in: the reader stays where they were, and the spinner goes
        hush.update(cx, |app, cx| {
            app.older_loaded();
            app.set_messages(all[100..].to_vec(), cx);
        });
        draw(window, cx);
        assert_eq!(top(&hush, cx), all[202].key());
        assert!(!loading_older(&hush, cx));

        // the next page is short: the whole chat is in, nothing older is asked for
        scroll_to(&hush, 5, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), 3 * PAGE);
        hush.update(cx, |app, cx| {
            app.older_loaded();
            app.set_messages(all[50..].to_vec(), cx);
        });
        draw(window, cx);
        scroll_to(&hush, 0, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), 3 * PAGE);
        assert!(!loading_older(&hush, cx));
    }

    #[gpui::test]
    fn a_page_or_a_changed_message_reads_only_its_own_details(cx: &mut TestAppContext) {
        let (_, hush) = open(cx);
        let all = crate::demo::history(300);
        let asked = |hush: &Entity<Hush>, cx: &mut TestAppContext| hush.update(cx, |app, _| app.take_wanted());
        let keys = |ms: &[Message]| ms.iter().map(Message::key).collect::<Vec<_>>();
        show(&hush, &all[200..], cx);
        assert_eq!(asked(&hush, cx), keys(&all[200..]));
        // an older page: its rows only
        show(&hush, &all[100..], cx);
        assert_eq!(asked(&hush, cx), keys(&all[100..200]));
        // a message that changed, a file that did
        let changed = |chat: &str, k: MessageKey| CoreEvent::MessageChanged {
            conversation_id: chat.into(),
            key: k,
        };
        hush.update(cx, |app, cx| app.handle(changed("g:01", all[250].key()), cx));
        assert_eq!(asked(&hush, cx), [all[250].key()]);
        hush.update(cx, |app, cx| {
            let file = CoreEvent::AttachmentChanged {
                conversation_id: "g:01".into(),
                message: all[260].key(),
                id: 7,
            };
            app.handle(file, cx)
        });
        assert_eq!(asked(&hush, cx), [all[260].key()]);
        // another chat's, or a message not loaded: nothing
        hush.update(cx, |app, cx| app.handle(changed("g:02", all[270].key()), cx));
        hush.update(cx, |app, cx| app.handle(changed("g:01", all[3].key()), cx));
        assert!(asked(&hush, cx).is_empty());
        // back to the newest: none read again
        show(&hush, &all[200..], cx);
        assert!(asked(&hush, cx).is_empty());
    }

    #[gpui::test]
    fn no_page_is_asked_for_while_a_load_is_on_its_way(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        let all = crate::demo::history(100);
        show(&hush, &all, cx);
        draw(window, cx);
        // back at the newest, say: its reload still running would move the reader
        hush.update(cx, |app, _| app.messages_loading = Some(app.generation));
        scroll_to(&hush, 5, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), PAGE);
        // in: scrolling on asks
        hush.update(cx, |app, _| app.messages_loading = None);
        scroll_to(&hush, 4, cx);
        draw(window, cx);
        assert_eq!(depth(&hush, cx), 2 * PAGE);
    }

    #[gpui::test]
    fn a_long_history_draws_as_fast_as_a_short_one(cx: &mut TestAppContext) {
        let (window, hush) = open(cx);
        // the fastest of a few frames: what drawing costs, without the machine's other work
        let frame = |n: usize, cx: &mut TestAppContext| -> Duration {
            show(&hush, &crate::demo::history(n), cx);
            draw(window, cx);
            let frames = (0..5).map(|_| {
                let start = Instant::now();
                draw(window, cx);
                start.elapsed()
            });
            frames.min().expect("five frames")
        };
        let short = frame(100, cx);
        let long = frame(5_000, cx);
        eprintln!("a frame with 100 messages loaded: {short:?}; with 5,000: {long:?}");
        assert!(long < short * 2, "100 messages: {short:?}; 5,000: {long:?}");
    }
}
