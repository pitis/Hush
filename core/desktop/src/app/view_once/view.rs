//! How view-once media is drawn: its button or words in the bubble (`ConversationView.viewOnce`),
//! and the look over the window (the Mac app's Quick Look).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::h_flex;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{label, Look, State};
use crate::app::view::{card, icon, modal};
use crate::app::{Hush, Screen};
use crate::style::Palette;

/// The bubble's button ("View photo", "Download view-once media"), or its words. `color` is the
/// bubble's words'.
pub fn bubble(state: &Option<State>, this: &WeakEntity<Hush>, color: Hsla) -> Option<AnyElement> {
    let state = state.as_ref()?;
    let says = label(state);
    let content = || {
        h_flex()
            .gap(px(6.))
            .text_size(px(14.))
            .child(icon("circle-dashed", 14., color))
            .child(says)
    };
    let button = |id: &'static str| {
        content()
            .id(id)
            .role(Role::Button)
            .aria_label(says)
            .px(px(10.))
            .py(px(4.))
            .rounded(px(8.))
            .border_1()
            .border_color(color.opacity(0.5))
    };
    let this = this.clone();
    Some(match state.clone() {
        State::Open(file) => button("view-once")
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                let _ = this.update(cx, |app, cx| app.view_once(&file, window, cx));
            })
            .into_any_element(),
        // one look: a second click would read it again
        State::Viewing(_) => button("view-once").opacity(0.5).into_any_element(),
        State::Download(file) => button("view-once")
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let _ = this.update(cx, |app, cx| app.download(&file, cx));
            })
            .into_any_element(),
        State::Sent | State::Viewed | State::Downloading | State::Unavailable => content().italic().into_any_element(),
    })
}

impl Hush {
    /// The look over the window: the photo as large as it fits, or the video's player said; Close
    /// or Esc ends it.
    pub(in crate::app) fn render_view_once(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let viewer = self
            .view_once
            .viewer
            .as_ref()
            .filter(|_| matches!(self.screen, Screen::Chats))?;
        let close = cx.listener(|this, _, window, cx| this.close_view_once(window, cx));
        let escape = cx.listener(|this, e: &KeyDownEvent, window, cx| {
            if e.keystroke.key == "escape" {
                this.close_view_once(window, cx);
            }
        });
        Some(match &viewer.look {
            Look::Photo(photo) => div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(black().opacity(0.92))
                .child(
                    div()
                        .id("view-once-photo")
                        .role(Role::Dialog)
                        .aria_label("View-once photo")
                        .track_focus(&viewer.focus)
                        .on_key_down(escape)
                        .size_full()
                        .p(px(48.))
                        .flex()
                        .flex_col()
                        // its height is the window's, not the photo's
                        .child(img(photo.clone()).flex_1().min_h_0().w_full().object_fit(ObjectFit::Contain)),
                )
                .child(
                    div().absolute().top(px(12.)).right(px(12.)).child(
                        Button::new("view-once-close")
                            .ghost()
                            .icon(icon("x", 18., white()))
                            .accessibility_label("Close")
                            .on_click(close),
                    ),
                ),
            Look::Video(_) => modal(
                card(p)
                    .id("view-once-video")
                    .role(Role::Dialog)
                    .aria_label("View-once video")
                    .track_focus(&viewer.focus)
                    .on_key_down(escape)
                    .w(px(360.))
                    .items_center()
                    .child(icon("film", 26., p.secondary))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("View-once video"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.secondary)
                            .text_center()
                            .child("It's playing in your video player. Close this when you're done: the video is removed from this computer then."),
                    )
                    .child(
                        Button::new("view-once-close")
                            .primary()
                            .label("Close")
                            .on_click(close),
                    ),
            ),
        })
    }
}
