//! What the composer adds (the Mac app's `ComposerField` and `MentionList`): the field with its
//! mentions and Format menu, and above it the mention picker and the styled text's preview.

use gpui_kit::component::input::{
    Copy as CopyText, Cut, Enter, Escape, IndentInline, MoveDown, MoveUp, Paste, SelectAll, Textarea,
};
use gpui_kit::component::native_menu::NativeMenu;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Bold, Italic, Monospace, Picker, Spoiler, Strikethrough};
use crate::app::actions::view::styled;
use crate::app::view::avatar;
use crate::app::Hush;
use crate::style::Palette;
use crate::text::{BOLD, ITALIC, MONOSPACE, SPOILER, STRIKETHROUGH};

impl Hush {
    /// The message field: a mention reads "@Name" in the accent colour; its context menu has
    /// Format (`ComposerTextView.menu`).
    pub(in crate::app) fn composer_field(&self, p: Palette) -> Textarea {
        let composer = self.composer.clone();
        Textarea::new(&self.composer)
            .appearance(false)
            .aria_label("Message")
            .token(move |token, _, cx| {
                div()
                    .h(token.line_height())
                    .flex()
                    .items_center()
                    .text_color(p.accent)
                    .when(token.is_selected(), |this| this.bg(cx.theme().selection))
                    .child(token.token().label().clone())
            })
            .context_menu(move |menu, _, cx| {
                let nothing = composer.read(cx).selected_range().is_empty();
                let format = NativeMenu::new()
                    .menu("Bold", Box::new(Bold))
                    .menu("Italic", Box::new(Italic))
                    .menu("Strikethrough", Box::new(Strikethrough))
                    .menu("Monospace", Box::new(Monospace))
                    .menu("Spoiler", Box::new(Spoiler));
                menu.submenu("Format", format)
                    .separator()
                    .menu_with_disabled("Cut", nothing, Box::new(Cut))
                    .menu_with_disabled("Copy", nothing, Box::new(CopyText))
                    .menu("Paste", Box::new(Paste))
                    .separator()
                    .menu("Select All", Box::new(SelectAll))
            })
    }

    /// `composer` (with its bars) under the mention picker and the styled text's preview. It
    /// takes the format shortcuts and, while the picker is open, ↑ ↓ to choose, ↩ or Tab to pick
    /// and Esc to close it (before the field, the reply's Esc and ↑'s edit).
    pub(in crate::app) fn with_compose(&self, composer: Div, p: Palette, cx: &mut Context<Self>) -> Div {
        let picker = self
            .picker(cx)
            .map(|picker| panel(self.render_picker(picker, p, cx), p));
        let preview = self.render_preview(p, cx).map(|preview| panel(preview, p));
        v_flex()
            .flex_none()
            .on_action(cx.listener(|this, _: &Bold, _, cx| this.toggle_style(BOLD, cx)))
            .on_action(cx.listener(|this, _: &Italic, _, cx| this.toggle_style(ITALIC, cx)))
            .on_action(cx.listener(|this, _: &Strikethrough, _, cx| this.toggle_style(STRIKETHROUGH, cx)))
            .on_action(cx.listener(|this, _: &Monospace, _, cx| this.toggle_style(MONOSPACE, cx)))
            .on_action(cx.listener(|this, _: &Spoiler, _, cx| this.toggle_style(SPOILER, cx)))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.move_mention(-1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.move_mention(1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                if this.pick_mention(None, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.pick_mention(None, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.dismiss_mentions(cx) {
                    cx.stop_propagation();
                }
            }))
            .children(picker)
            .children(preview)
            .child(composer)
    }

    /// The members matching the "@word", 22pt pictures by their names; a click picks one.
    fn render_picker(&self, picker: Picker, p: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let rows = picker.matches.into_iter().enumerate().map(|(i, (aci, name))| {
            let chosen = i == picker.selected;
            h_flex()
                .id(SharedString::from(format!("mention-{aci}")))
                .role(Role::ListItem)
                .aria_label(name.clone())
                .aria_selected(chosen)
                .gap(px(8.))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(chosen, |this| this.bg(p.accent.opacity(0.3)))
                .child(avatar(&format!("c:{aci}"), &name, None, 22.))
                .child(div().text_size(px(13.)).child(name))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick_mention(Some(i), window, cx);
                }))
        });
        v_flex()
            .id("mentions")
            .role(Role::List)
            .aria_label("Mention someone")
            .children(rows)
    }

    /// The text as it goes out, while any of it is styled: the field can't draw its styles.
    fn render_preview(&self, p: Palette, cx: &App) -> Option<Stateful<Div>> {
        let (text, runs) = self.preview(cx)?;
        let shade = HighlightStyle {
            background_color: Some(p.secondary.opacity(0.3)),
            ..Default::default()
        };
        Some(
            v_flex()
                .id("formatted")
                .role(Role::Group)
                .aria_label("Formatted")
                .gap(px(1.))
                .px(px(4.))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.secondary)
                        .child("Formatted"),
                )
                .child(div().text_size(px(14.)).line_clamp(3).child(styled(
                    text,
                    &runs,
                    shade,
                    p.accent,
                    &cx.theme().mono_font_family,
                ))),
        )
    }
}

/// One of the composer's accessories, in its panel above the field.
fn panel(content: impl IntoElement, p: Palette) -> Div {
    div()
        .px(px(14.))
        .pt(px(8.))
        .child(div().p(px(8.)).rounded(px(16.)).bg(p.field).child(content))
}
