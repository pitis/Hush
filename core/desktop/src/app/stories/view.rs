//! What the stories draw: the Stories button by New Chat (`SidebarView.swift`), and their window
//! over the chats, the chats on the left and the story on the right (`StoriesView.swift`'s sizes).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, Disableable, FocusTrapElement};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Attachment, AttachmentState, Story, StoryText};

use super::{step, unseen_count, Group};
use crate::app::files::{self, Look};
use crate::app::previews::https;
use crate::app::view::{avatar, icon, label, modal};
use crate::app::Hush;
use crate::style::Palette;
use crate::text;

/// A text story's background when it names none (`StoryCard.background`).
const PLAIN: u32 = 0xFF334566;

impl Hush {
    /// The sidebar's Stories button while there are stories; it says how many are new.
    pub(in crate::app) fn render_stories_button(&self, p: Palette, cx: &mut Context<Self>) -> Option<Button> {
        if self.stories.list.is_empty() {
            return None;
        }
        let new = unseen_count(&self.stories.list, self.own_aci.as_deref());
        let (said, symbol) = match new {
            0 => ("Stories".to_string(), "circle-dashed"),
            n => (format!("Stories, {n} new"), "circle-dot-dashed"),
        };
        let button = Button::new("stories")
            .ghost()
            .icon(icon(symbol, 15., p.secondary))
            .accessibility_label(said.clone())
            .tooltip(said)
            .on_click(cx.listener(|this, _, window, cx| this.open_stories(window, cx)));
        Some(button)
    }

    /// The stories window over the chats, while open.
    pub(in crate::app) fn render_stories(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let v = self.stories.viewer.as_ref()?;
        let groups = self.story_groups();
        let now = chrono::Local::now();
        let rows = groups.iter().enumerate().map(|(ix, g)| {
            let selected = v.chat.as_deref() == Some(g.chat);
            self.render_story_chat(ix, g, selected, now, p, cx)
        });
        let list = v_flex()
            .id("story-chats")
            .role(Role::List)
            .aria_label("Chats with stories")
            .w(px(220.))
            .flex_none()
            .h_full()
            .py(px(8.))
            .overflow_y_scroll()
            .children(rows);
        let story = self.current_story();
        let can = |by| step(&groups, v.chat.as_deref(), v.index, by).is_some();
        // the icon's own colour overrides a disabled button's
        let arrow = |by| if can(by) { p.text } else { p.secondary.opacity(0.4) };
        let close = Button::new("stories-close")
            .ghost()
            .icon(icon("x", 14., p.secondary))
            .accessibility_label("Close")
            .on_click(cx.listener(|this, _, window, cx| this.close_stories(window, cx)));
        let header = h_flex().h(px(28.)).flex_none().gap(px(8.));
        let header = match story {
            Some(s) => header
                .child(
                    label("story-author", Role::Heading, self.name(&s.author))
                        .aria_level(2)
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    label("story-time", Role::Label, text::list_time(s.sent_at, now))
                        .text_size(px(12.))
                        .text_color(p.secondary),
                )
                .child(div().flex_1())
                .child(
                    Button::new("story-previous")
                        .ghost()
                        .icon(icon("chevron-left", 14., arrow(-1)))
                        .accessibility_label("Previous story")
                        .disabled(!can(-1))
                        .on_click(cx.listener(|this, _, _, cx| this.step_story(-1, cx))),
                )
                .child(
                    Button::new("story-next")
                        .ghost()
                        .icon(icon("chevron-right", 14., arrow(1)))
                        .accessibility_label("Next story")
                        .disabled(!can(1))
                        .on_click(cx.listener(|this, _, _, cx| this.step_story(1, cx))),
                ),
            None => header.child(div().flex_1()),
        };
        let shown = match story {
            Some(s) => self.render_story(s, p, cx),
            None => label("no-stories", Role::Status, "No stories")
                .text_color(p.secondary)
                .into_any_element(),
        };
        let viewer = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p(px(16.))
            .gap(px(10.))
            .child(header.child(close))
            // what went wrong with a file or a link here, where it's seen
            .children(self.error.clone().map(|e| self.render_error(e, p, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(shown),
            );
        Some(modal(
            h_flex()
                .id("stories")
                .role(Role::Dialog)
                .aria_label("Stories")
                .w(px(720.))
                .h(px(520.))
                .max_h(relative(0.96))
                .rounded(px(12.))
                .overflow_hidden()
                .bg(p.window)
                .border_1()
                .border_color(p.separator)
                .on_key_down(
                    cx.listener(|this, e: &KeyDownEvent, window, cx| match e.keystroke.key.as_str() {
                        "escape" => this.close_stories(window, cx),
                        "left" => this.step_story(-1, cx),
                        "right" => this.step_story(1, cx),
                        _ => {}
                    }),
                )
                .child(list)
                .child(div().w(px(1.)).h_full().flex_none().bg(p.separator))
                .child(viewer)
                .focus_trap("stories", &v.focus),
        ))
    }

    /// A chat in the list: its picture ringed while it has a story not seen, its name, its newest
    /// story's time.
    fn render_story_chat(
        &self,
        ix: usize,
        g: &Group,
        selected: bool,
        now: chrono::DateTime<chrono::Local>,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let name = self.story_chat_name(g);
        let unseen = g.unseen();
        let chat = self.chats.iter().find(|c| c.id == g.chat);
        // a blocked chat's or a request's picture isn't shown (a stranger chose it)
        let picture = chat
            .filter(|c| !c.blocked && !c.message_request)
            .and_then(|c| c.avatar.as_deref())
            .and_then(|path| self.picture(path));
        let id = g.chat.to_owned();
        let ring = if unseen { p.bubble_out } else { transparent_black() };
        div()
            .id(("story-chat", ix))
            .role(Role::ListItem)
            .aria_label(if unseen { format!("{name}, new") } else { name.clone() })
            .aria_selected(selected)
            .px(px(8.))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.show_story_chat(id.clone(), cx)))
            .child(
                h_flex()
                    .h(px(48.))
                    .px(px(6.))
                    .gap(px(10.))
                    .rounded(px(8.))
                    .when(selected, |this| this.bg(p.selection))
                    .child(
                        div()
                            .p(px(3.))
                            .rounded_full()
                            .border_2()
                            .border_color(ring)
                            .child(avatar(g.chat, &name, picture, 30.)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(if unseen {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.secondary)
                                    .child(text::list_time(g.newest(), now)),
                            ),
                    ),
            )
    }

    /// What a chat's stories are listed as: "My stories" for ours outside a group, else the
    /// chat's name, else the author's (`StoriesView.name`).
    fn story_chat_name(&self, g: &Group) -> String {
        let me = self.own_aci.as_deref();
        if g.ours(me) && !g.chat.starts_with("g:") {
            return "My stories".into();
        }
        match self.chats.iter().find(|c| c.id == g.chat) {
            Some(c) => text::title(c, me),
            None => self.name(&g.stories[0].author),
        }
    }

    /// One story: its words on their background, or its photo or video (`StoryCard`).
    fn render_story(&self, s: &Story, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        if let Some(t) = &s.text {
            return self.render_story_text(t, cx).into_any_element();
        }
        let Some(a) = &s.file else { return cant_show(p) };
        match a.state {
            AttachmentState::Ready if a.content_type.starts_with("video/") => {
                self.render_story_video(a, &s.conversation_id, cx)
            }
            AttachmentState::Ready if files::is_image(a) => self.render_story_photo(a, p, cx),
            AttachmentState::Ready => cant_show(p),
            AttachmentState::Downloading => Spinner::new().into_any_element(),
            _ if !a.available => cant_show(p),
            // fetched as it shows (`story_shown`)
            AttachmentState::Pending => Spinner::new().into_any_element(),
            AttachmentState::Failed => {
                let file = a.clone();
                Button::new("story-retry")
                    .label("Retry")
                    .on_click(cx.listener(move |this, _, _, cx| this.download(&file, cx)))
                    .into_any_element()
            }
        }
    }

    /// The words on their colour or gradient, in their font, with their link under them.
    fn render_story_text(&self, t: &StoryText, cx: &mut Context<Self>) -> Div {
        let (family, size, weight) = font(t.style);
        let words = label("story-text", Role::Label, t.text.clone())
            .max_w_full()
            .text_center()
            .text_size(px(size))
            .font_weight(weight)
            .text_color(t.foreground.map_or(white(), argb))
            .when_some(family, |this, family| this.font_family(family))
            .when_some(t.text_background, |this, behind| {
                this.p(px(8.)).rounded(px(8.)).bg(argb(behind))
            });
        // https only, as the Mac app opens
        let link = t.link.as_deref().and_then(https).map(|url| {
            let host = url.host_str().unwrap_or_default().to_owned();
            div()
                .id("story-link")
                .role(Role::Link)
                .aria_label(host.clone())
                .cursor_pointer()
                .px(px(12.))
                .py(px(6.))
                .rounded_full()
                .bg(black().opacity(0.3))
                .text_color(white())
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .child(host)
                .on_click(cx.listener(move |this, _, _, cx| this.open_link(url.as_str(), cx)))
        });
        card().bg(background(t)).child(
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(12.))
                .p(px(32.))
                .child(words)
                .children(link),
        )
    }

    /// A photo story through the bubbles' picture cache and its budget: decoded once drawn.
    fn render_story_photo(&self, a: &Attachment, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let shown = match self.files.look(a) {
            Look::Picture(Some(picture)) => div()
                .id("story-photo")
                .role(Role::Image)
                .aria_label("Photo")
                .size_full()
                .child(img(picture).size_full().rounded(px(14.)).object_fit(ObjectFit::Contain))
                .into_any_element(),
            Look::Picture(None) => Spinner::new().into_any_element(),
            // it didn't decode
            _ => cant_show(p),
        };
        if self.files.wants_pictures() {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, |app, cx| app.decode_pictures(cx));
            });
        }
        shown
    }

    /// A video story opens in the system's player, as a chat's videos do.
    /// shortcut: the Mac plays it in the window (`StoryVideo`), GPUI plays no video; play it here
    /// once gpui-kit can.
    fn render_story_video(&self, a: &Attachment, chat: &str, cx: &mut Context<Self>) -> AnyElement {
        let (file, chat) = (a.clone(), chat.to_owned());
        card()
            .id("story-video")
            .role(Role::Button)
            .aria_label("Play video")
            .cursor_pointer()
            .bg(black())
            .child(
                v_flex()
                    .items_center()
                    .gap(px(10.))
                    .text_color(white())
                    .text_size(px(13.))
                    .child(files::disc("play", 56., white(), black()))
                    .child("Play video"),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open_file_of(chat.clone(), &file, cx)))
            .into_any_element()
    }
}

/// A text or video story's frame: 9:16, as tall as it can be, its corners round.
fn card() -> Div {
    div()
        .h_full()
        .max_w_full()
        .aspect_ratio(9. / 16.)
        .flex_none()
        .rounded(px(14.))
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center()
}

fn cant_show(p: Palette) -> AnyElement {
    label("story-unshown", Role::Status, "This story can't be shown")
        .text_color(p.secondary)
        .into_any_element()
}

/// An ARGB colour.
fn argb(c: u32) -> Hsla {
    rgba(c.rotate_left(8)).into()
}

/// One colour, or a gradient at Signal's angle (0 bottom to top, 90 left to right: CSS's, as
/// GPUI's); none: the default.
/// shortcut: a gradient of more colours shows its first and last (GPUI's have two stops); add
/// bands if a story's middle colours go missing.
fn background(t: &StoryText) -> Background {
    match t.background.as_slice() {
        [] => argb(PLAIN).into(),
        [one] => argb(*one).into(),
        [first, .., last] => linear_gradient(
            t.angle as f32,
            linear_color_stop(argb(*first), 0.),
            linear_color_stop(argb(*last), 1.),
        ),
    }
}

/// A text story's font: its family (None: the system's), size and weight (`StoryCard.font`;
/// Signal's TextAttachment.Style).
/// shortcut: condensed (5) is only semibold, GPUI picks no font width; serif is Georgia (the Mac's
/// is New York, not reachable by name) and DejaVu Serif, which not every Linux has; and Linux has
/// no script font everywhere, so it gets the system's. A missing family falls back to GPUI's.
fn font(style: u32) -> (Option<&'static str>, f32, FontWeight) {
    match style {
        2 => (None, 28., FontWeight::BOLD),
        3 => (Some(SERIF), 28., FontWeight::NORMAL),
        4 => (SCRIPT, 32., FontWeight::NORMAL),
        5 => (None, 28., FontWeight::SEMIBOLD),
        _ => (None, 28., FontWeight::NORMAL),
    }
}

#[cfg(any(target_os = "macos", windows))]
const SERIF: &str = "Georgia";
#[cfg(not(any(target_os = "macos", windows)))]
const SERIF: &str = "DejaVu Serif";
#[cfg(target_os = "macos")]
const SCRIPT: Option<&str> = Some("Snell Roundhand");
#[cfg(windows)]
const SCRIPT: Option<&str> = Some("Segoe Script");
#[cfg(not(any(target_os = "macos", windows)))]
const SCRIPT: Option<&str> = None;

#[cfg(test)]
mod tests {
    use super::{argb, font};
    use gpui_kit::{rgba, FontWeight, Hsla};

    #[test]
    fn colours_are_argb_and_fonts_take_the_macs_sizes_and_weights() {
        let red: Hsla = rgba(0xff000080).into();
        assert_eq!(argb(0x80ff0000), red, "alpha first");
        assert_eq!(font(2), (None, 28., FontWeight::BOLD));
        assert_eq!(font(0), font(1), "default and regular are the same");
        assert_eq!(font(4).1, 32., "script is larger");
    }
}
