//! What a chat's actions draw (the Mac app's `SidebarView` and `RequestBar`): its row's
//! right-click menu, the bar in place of the composer, and the way into the archive and back.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::{h_flex, v_flex, Disableable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::Conversation;

use super::{answers, mute_until, prompt, Item, MUTES};
use crate::app::view::{icon, label};
use crate::app::Hush;
use crate::style::Palette;
use crate::text;

impl Hush {
    /// A chat's row with its right-click menu: what it offers is read as it opens.
    pub(in crate::app) fn with_chat_menu(
        &self,
        row: Stateful<Div>,
        id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (this, id) = (cx.entity().downgrade(), id.to_owned());
        row.context_menu(move |mut menu, window, cx| {
            let Some(app) = this.upgrade() else { return menu };
            let items = {
                let app = app.read(cx);
                let Some(c) = app.chats.iter().find(|c| c.id == id) else {
                    return menu;
                };
                let pinned = app.chats.iter().filter(|c| c.pinned_order.is_some()).count();
                super::menu(c, app.own_aci.as_deref(), pinned, chrono::Utc::now().timestamp_millis())
            };
            for item in items {
                let pick = {
                    let (this, id) = (this.clone(), id.clone());
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |app, cx| app.pick(item, id.clone(), window, cx));
                    }
                };
                if matches!(item, Item::Archive | Item::Unarchive) {
                    menu = menu.separator();
                }
                menu = match item {
                    Item::Mute => {
                        let (this, id) = (this.clone(), id.clone());
                        menu.submenu(item.label(), window, cx, move |menu, _, _| {
                            MUTES.iter().fold(menu, |menu, &(title, hours)| {
                                let (this, id) = (this.clone(), id.clone());
                                menu.item(PopupMenuItem::new(title).on_click(move |_, _, cx| {
                                    let until = mute_until(hours, chrono::Utc::now().timestamp_millis());
                                    let _ = this.update(cx, |app, cx| app.mute(id.clone(), until, cx));
                                }))
                            })
                        })
                    }
                    Item::Pin { allowed } => {
                        menu.item(PopupMenuItem::new(item.label()).disabled(!allowed).on_click(pick))
                    }
                    _ => menu.item(PopupMenuItem::new(item.label()).on_click(pick)),
                };
            }
            menu
        })
    }

    /// In place of the composer, as Desktop's: a message request's, an invitation's, our join
    /// request's or a blocked chat's buttons, the default one last (`RequestBar`).
    pub(in crate::app) fn render_request_bar(&self, c: &Conversation, p: Palette, cx: &mut Context<Self>) -> Div {
        let name = text::title(c, self.own_aci.as_deref());
        let shown = answers(c);
        let buttons = shown.iter().enumerate().map(|(i, &a)| {
            let id = c.id.clone();
            Button::new(("answer", i))
                .label(a.label())
                .when(i + 1 == shown.len(), |b| b.primary())
                .disabled(self.answering && a.waits())
                .on_click(cx.listener(move |this, _, window, cx| this.answer(a, id.clone(), window, cx)))
        });
        v_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(18.))
            .py(px(14.))
            .child(
                label("request-prompt", Role::Label, prompt(c, &name))
                    .text_center()
                    .text_color(p.secondary),
            )
            .child(h_flex().gap(px(10.)).children(buttons))
    }

    /// The way into the archive, under the inbox (`SidebarView.archiveRow`).
    pub(in crate::app) fn render_archive_row(&self, p: Palette, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let count = self.chats.iter().filter(|c| c.archived).count();
        if count == 0 || self.chat_actions.archive || !self.query.is_empty() || self.find.scope.is_some() {
            return None;
        }
        let row = h_flex()
            .h(px(32.))
            .px(px(8.))
            .gap(px(8.))
            .child(icon("archive", 15., p.secondary))
            .child(div().flex_1().child("Archived Chats"))
            .child(div().text_color(p.secondary).child(count.to_string()));
        Some(
            div()
                .id("archived-chats")
                .role(Role::Button)
                .aria_label(format!("Archived Chats, {count}"))
                .px(px(10.))
                .pt(px(4.))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.show_archive(true, cx)))
                .child(row),
        )
    }

    /// Out of the archive, in New Chat's place (the Mac app's toolbar).
    pub(in crate::app) fn render_archive_back(&self, p: Palette, cx: &mut Context<Self>) -> Button {
        Button::new("back-to-chats")
            .ghost()
            .icon(icon("chevron-left", 15., p.secondary))
            .accessibility_label("Back to Chats")
            .tooltip("Back to chats")
            .on_click(cx.listener(|this, _, _, cx| this.show_archive(false, cx)))
    }
}
