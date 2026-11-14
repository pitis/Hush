//! The sticker picker, over "+" (`StickerPicker`'s 340×360).

use std::sync::Arc;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{h_flex, v_flex, Disableable as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{can_add, shown, Picker, PICKER_PIXELS};
use crate::app::actions::view::Trigger;
use crate::app::view::{field, label};
use crate::app::Hush;
use crate::style::Palette;

/// What the picker says with no packs (the Mac app's words).
const NO_PACKS: &str = "No sticker packs yet. Packs added on your phone show here, or paste a pack's link below.";

impl Hush {
    /// The picker, opening above "+" when open: the packs' tabs, the shown pack's stickers four to
    /// a row, and a pack's link to add it.
    pub(in crate::app) fn render_stickers(&self, p: Palette, cx: &mut Context<Self>) -> Popover {
        let content = self
            .open_picker()
            .map(|picker| self.render_sticker_picker(picker, p, cx));
        // its pictures were drawn: those not decoded yet are, as the timeline's
        let this = cx.entity().downgrade();
        if self.files.wants_pictures() {
            let this = this.clone();
            cx.defer(move |cx| {
                let _ = this.update(cx, |app, cx| app.decode_pictures(cx));
            });
        }
        Popover::new("stickers")
            .anchor(Anchor::BottomLeft)
            .open(content.is_some())
            .on_open_change(move |open, window, cx| {
                if !*open {
                    let _ = this.update(cx, |app, cx| app.close_stickers(window, cx));
                }
            })
            // shown from "+"'s menu only: above "+", at its left edge
            .trigger(Trigger(div().id("stickers-anchor")))
            .when_some(content, |popover, content| popover.child(content))
    }

    fn render_sticker_picker(&self, picker: &Picker, p: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let packs = &self.stickers.packs;
        let body = match shown(packs, picker.pack.as_deref()) {
            None => div().flex_1().flex().items_center().justify_center().p(px(20.)).child(
                label("no-stickers", Role::Label, NO_PACKS)
                    .text_center()
                    .text_size(px(12.))
                    .text_color(p.secondary),
            ),
            Some(pack) => {
                let tabs = packs.iter().enumerate().map(|(i, each)| {
                    let chosen = each.id == pack.id;
                    let first = each.stickers.first();
                    let picture = first.and_then(|s| self.files.picture_at(&s.path, PICKER_PIXELS));
                    let id = each.id.clone();
                    div()
                        .id(("sticker-pack", i))
                        .role(Role::Tab)
                        .aria_label(each.title.clone())
                        .aria_selected(chosen)
                        .flex_none()
                        .p(px(4.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(chosen, |tab| tab.bg(p.selection))
                        .child(sticker(picture, 28.))
                        .on_click(cx.listener(move |this, _, _, cx| this.choose_pack(id.clone(), cx)))
                });
                let cells = pack.stickers.iter().map(|s| {
                    let picture = self.files.picture_at(&s.path, PICKER_PIXELS);
                    let name = s.emoji.as_ref().map_or("Sticker".into(), |e| format!("Sticker {e}"));
                    let (pack, id) = (pack.id.clone(), s.id);
                    div()
                        .id(("sticker", s.id as u64))
                        .role(Role::Button)
                        .aria_label(name)
                        .flex_none()
                        .cursor_pointer()
                        .child(sticker(picture, 72.))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.send_sticker(pack.clone(), id, window, cx)),
                        )
                });
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        h_flex()
                            .id("sticker-packs")
                            .role(Role::TabList)
                            .aria_label("Sticker packs")
                            .flex_none()
                            .gap(px(4.))
                            .pb(px(8.))
                            .overflow_x_scroll()
                            .children(tabs),
                    )
                    .child(separator(p))
                    .child(
                        div()
                            .id("stickers")
                            .role(Role::Group)
                            .aria_label(pack.title.clone())
                            .flex_1()
                            .min_h_0()
                            .py(px(10.))
                            .overflow_y_scroll()
                            .child(h_flex().w(px(312.)).flex_wrap().gap(px(8.)).children(cells)),
                    )
            }
        };
        let typed = picker.link.read(cx).value();
        v_flex()
            .id("sticker-picker")
            .w(px(316.))
            .h(px(336.))
            .child(body)
            .child(separator(p))
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .pt(px(8.))
                    .child(
                        div().flex_1().child(field(
                            Input::new(&picker.link)
                                .appearance(false)
                                .aria_label("Sticker pack link")
                                .disabled(picker.adding)
                                .w_full(),
                            p,
                        )),
                    )
                    .child(
                        Button::new("add-sticker-pack")
                            .label("Add")
                            .disabled(!can_add(&typed, picker.adding))
                            .on_click(cx.listener(|this, _, window, cx| this.add_sticker_pack(window, cx))),
                    ),
            )
            .children(picker.problem.clone().map(|why| {
                label("sticker-problem", Role::Alert, why)
                    .pt(px(6.))
                    .text_size(px(11.))
                    .text_color(p.danger)
            }))
    }
}

/// A sticker's picture in a square of `side` points; greyed until it decodes (or if it doesn't,
/// as on the Mac).
fn sticker(picture: Option<Arc<RenderImage>>, side: f32) -> AnyElement {
    match picture {
        Some(picture) => img(picture)
            .size(px(side))
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => div().size(px(side)).bg(black().opacity(0.25)).into_any_element(),
    }
}

fn separator(p: Palette) -> Div {
    div().h(px(1.)).flex_none().bg(p.separator)
}
