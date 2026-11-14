//! What Group Info draws: the Mac app's sheet (`GroupInfo.swift`) over the window, and the sheets
//! over it.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, Disableable, FocusTrapElement, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind};
use sig_core::{GroupChange, GroupInfo, LinkAccess};

use super::{sorted, timer_choice, timer_choices, Panel, Sheet};
use crate::app::view::{avatar, card, field, icon, label, modal};
use crate::app::Hush;
use crate::style::Palette;
use crate::text;

impl Hush {
    /// The toolbar's Chat Options menu (ⓘ): Group Info… in a group.
    pub(in crate::app) fn chat_options(
        &self,
        c: &Conversation,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if c.kind != ConversationKind::Group {
            return None;
        }
        let (this, id) = (cx.entity().downgrade(), c.id.clone());
        let menu = Button::new("chat-options")
            .ghost()
            .icon(icon("info", 16., p.secondary))
            .accessibility_label("Chat Options")
            .tooltip("Chat options")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let (this, id) = (this.clone(), id.clone());
                menu.item(PopupMenuItem::new("Group Info…").on_click(move |_, window, cx| {
                    let _ = this.update(cx, |app, cx| app.open_group_info(id.clone(), window, cx));
                }))
            });
        Some(menu)
    }

    /// The Group Info panel over the window, and its sheet over that.
    pub(in crate::app) fn render_group_info(&self, p: Palette, cx: &mut Context<Self>) -> Vec<Div> {
        let Some(g) = &self.group else { return vec![] };
        let mut shown = vec![self.render_panel(g, p, cx)];
        shown.extend(g.sheet.as_ref().map(|s| self.render_sheet(g, s, p, cx)));
        shown
    }

    fn render_panel(&self, g: &Panel, p: Palette, cx: &mut Context<Self>) -> Div {
        let chat = self.chats.iter().find(|c| c.id == g.id);
        let title = chat.map_or("Group".into(), |c| text::title(c, self.own_aci.as_deref()));
        let body = match &g.info {
            Some(info) => self.render_group(g, info, chat, &title, p, cx).into_any_element(),
            None => {
                let says = match () {
                    _ if !g.loaded => "Loading…",
                    _ if g.failure.is_none() => "This group isn't known yet: it shows once a message arrives in it.",
                    _ => "Nothing to show.",
                };
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(20.))
                    .child(
                        label("group-unknown", Role::Status, says)
                            .text_center()
                            .text_color(p.secondary),
                    )
                    .into_any_element()
            }
        };
        let footer = h_flex()
            .flex_none()
            .gap(px(8.))
            .p(px(12.))
            .border_t_1()
            .border_color(p.separator)
            .when(g.busy, |this| this.child(Spinner::new().xsmall()))
            .children(g.failure.clone().map(|why| {
                label("group-failure", Role::Alert, why)
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(p.danger)
                    .line_clamp(2)
            }))
            .child(div().flex_1())
            .child(
                Button::new("group-done")
                    .primary()
                    .label("Done")
                    .on_click(cx.listener(|this, _, window, cx| this.close_group_info(window, cx))),
            );
        modal(
            v_flex()
                .id("group-info")
                .role(Role::Dialog)
                .aria_label("Group info")
                .w(px(400.))
                .h(px(540.))
                .rounded(px(12.))
                .overflow_hidden()
                .bg(p.window)
                .border_1()
                .border_color(p.separator)
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" {
                        this.close_group_info(window, cx);
                    }
                }))
                .child(body)
                .child(footer)
                .focus_trap("group-info", &g.focus),
        )
    }

    fn render_group(
        &self,
        g: &Panel,
        info: &GroupInfo,
        chat: Option<&Conversation>,
        title: &str,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let picture = chat
            .and_then(|c| c.avatar.as_deref())
            .and_then(|path| self.picture(path));
        let header = h_flex()
            .items_start()
            .gap(px(14.))
            .child(avatar(&g.id, title, picture.clone(), 64.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        label("group-title", Role::Heading, title.to_string())
                            .aria_level(1)
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .when(!info.description.is_empty(), |this| {
                        this.child(
                            label("group-description", Role::Label, info.description.clone())
                                .text_size(px(12.))
                                .text_color(p.secondary),
                        )
                    }),
            );
        let timer = chat.map_or(0, |c| c.expire_timer);
        let leave = if info.member {
            let title = title.to_string();
            h_flex()
                .child(
                    Button::new("leave-group")
                        .label("Leave Group…")
                        .text_color(p.danger)
                        .disabled(g.busy)
                        .on_click(cx.listener(move |this, _, window, cx| this.confirm_leave(&title, window, cx))),
                )
                .into_any_element()
        } else {
            label("not-member", Role::Label, "You're not in this group any more.")
                .text_size(px(12.))
                .text_color(p.secondary)
                .into_any_element()
        };
        v_flex()
            .id("group-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(20.))
            .gap(px(18.))
            .child(header)
            .when(info.can_edit_details, |this| {
                this.child(self.render_details(picture.is_some(), timer, g.busy, cx))
            })
            .child(self.render_members(info, g.busy, p, cx))
            .when(!info.invited.is_empty(), |this| {
                let invited = info.invited.iter().enumerate();
                this.child(
                    v_flex()
                        .gap(px(8.))
                        .child(heading("group-invited", format!("Invited ({})", info.invited.len())))
                        .children(invited.map(|(ix, id)| self.person(("invited", ix), id, false, p))),
                )
            })
            .when(info.admin && !info.requesting.is_empty(), |this| {
                this.child(self.render_requests(info, g.busy, p, cx))
            })
            .when(info.admin || info.link.is_some(), |this| {
                this.child(self.render_group_link(info, g, p, cx))
            })
            .child(leave)
    }

    /// Edit Name and Description…, the picture, and the timer: for those the group lets change them.
    fn render_details(&self, has_picture: bool, timer: u32, busy: bool, cx: &mut Context<Self>) -> Div {
        let this = cx.entity().downgrade();
        let picture =
            Button::new("group-picture")
                .label("Picture")
                .dropdown_caret(true)
                .disabled(busy)
                .dropdown_menu(move |menu, _, _| {
                    let choose = this.clone();
                    let menu = menu.item(PopupMenuItem::new("Choose…").disabled(busy).on_click(
                        move |_, window, cx| {
                            let _ = choose.update(cx, |app, cx| app.choose_group_picture(window, cx));
                        },
                    ));
                    if !has_picture {
                        return menu;
                    }
                    let remove = this.clone();
                    menu.item(PopupMenuItem::new("Remove").disabled(busy).on_click(move |_, _, cx| {
                        let image = vec![];
                        let _ = remove.update(cx, |app, cx| app.change_group(vec![GroupChange::Avatar { image }], cx));
                    }))
                });
        let timers = timer_choices(timer).into_iter().map(|s| (s, timer_choice(s))).collect();
        v_flex()
            .gap(px(12.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("group-edit")
                            .label("Edit Name and Description…")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.edit_group_details(window, cx))),
                    )
                    .child(picture),
            )
            .child(picker(
                ("group-timer", "Disappearing messages"),
                timer,
                timers,
                busy,
                |app, seconds, cx| app.change_group(vec![GroupChange::Timer { seconds }], cx),
                cx,
            ))
    }

    fn render_members(&self, info: &GroupInfo, busy: bool, p: Palette, cx: &mut Context<Self>) -> Div {
        let count = match info.members.len() {
            1 => "1 member".to_string(),
            n => format!("{n} members"),
        };
        let me = self.own_aci.as_deref();
        let rows: Vec<AnyElement> = sorted(&info.members, me, |aci| self.name(aci))
            .into_iter()
            .enumerate()
            .map(|(ix, m)| {
                let row = self.person(("member", ix), &m.aci, m.admin, p);
                // admins manage the others; no one demotes or removes themselves (they leave)
                if !info.admin || Some(m.aci.as_str()) == me {
                    return row.into_any_element();
                }
                let menu = member_menu(m.aci.clone(), m.admin, busy, cx.entity().downgrade());
                let more = Button::new(("manage", ix))
                    .ghost()
                    .icon(icon("ellipsis", 14., p.secondary))
                    .accessibility_label(format!("Manage {}", self.name(&m.aci)))
                    .disabled(busy)
                    .dropdown_menu_with_anchor(Anchor::TopRight, menu.clone());
                let row = row.child(more);
                if busy {
                    row.into_any_element()
                } else {
                    row.context_menu(menu).into_any_element()
                }
            })
            .collect();
        v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .child(heading("group-members", count))
                    .child(div().flex_1())
                    .when(info.can_add_members, |this| {
                        this.child(
                            Button::new("group-add")
                                .label("Add Members…")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| this.add_group_members(window, cx))),
                        )
                    }),
            )
            .children(rows)
    }

    /// People who asked to join through the link, for admins to let in or turn down.
    fn render_requests(&self, info: &GroupInfo, busy: bool, p: Palette, cx: &mut Context<Self>) -> Div {
        let rows = info.requesting.iter().enumerate().map(|(ix, aci)| {
            let (deny, approve) = (aci.clone(), aci.clone());
            self.person(("requesting", ix), aci, false, p)
                .child(
                    Button::new(("deny", ix))
                        .label("Deny")
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let aci = deny.clone();
                            this.change_group(vec![GroupChange::DenyRequest { aci }], cx)
                        })),
                )
                .child(
                    Button::new(("approve", ix))
                        .label("Approve")
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let aci = approve.clone();
                            this.change_group(vec![GroupChange::ApproveRequest { aci }], cx)
                        })),
                )
        });
        let count = format!("Requests to join ({})", info.requesting.len());
        v_flex()
            .gap(px(8.))
            .child(heading("group-requests", count))
            .children(rows.collect::<Vec<_>>())
    }

    /// The invite link: admins turn it on or off and reset it; members copy it while it's on.
    fn render_group_link(&self, info: &GroupInfo, g: &Panel, p: Palette, cx: &mut Context<Self>) -> Div {
        let access = [
            (LinkAccess::Off, "Off"),
            (LinkAccess::Anyone, "Anyone with the link"),
            (LinkAccess::Approval, "Admins approve who joins"),
        ];
        let access = access.map(|(a, s)| (a, s.to_string())).to_vec();
        let busy = g.busy;
        v_flex()
            .gap(px(8.))
            .child(heading("group-link", "Group link".into()))
            .when(info.admin, |this| {
                this.child(picker(
                    ("group-link-access", "Who can join"),
                    info.link_access,
                    access,
                    busy,
                    |app, access, cx| app.change_group(vec![GroupChange::LinkAccess { access }], cx),
                    cx,
                ))
            })
            .when(info.link.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Button::new("copy-link")
                                .label("Copy Link")
                                .on_click(cx.listener(|this, _, _, cx| this.copy_group_link(cx))),
                        )
                        .when(info.admin, |this| {
                            this.child(
                                Button::new("reset-link")
                                    .label("Reset Link…")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| this.confirm_reset_link(window, cx))),
                            )
                        })
                        .when(g.copied, |this| {
                            this.child(
                                label("link-copied", Role::Status, "Copied")
                                    .text_size(px(12.))
                                    .text_color(p.secondary),
                            )
                        }),
                )
            })
    }

    /// A person in the group: their picture, name and role (`GroupInfoView.row`).
    fn person(&self, id: impl Into<ElementId>, aci: &str, admin: bool, p: Palette) -> Stateful<Div> {
        let name = self.name(aci);
        let chat = format!("c:{aci}");
        // a blocked chat's or a request's picture isn't shown (a stranger chose it)
        let picture = self
            .chats
            .iter()
            .find(|c| c.id == chat && !c.blocked && !c.message_request)
            .and_then(|c| c.avatar.as_deref())
            .and_then(|path| self.picture(path));
        h_flex()
            .id(id)
            .role(Role::ListItem)
            .aria_label(if admin { format!("{name}, admin") } else { name.clone() })
            // as tall with a button beside it as without
            .min_h(px(32.))
            .gap(px(10.))
            .child(avatar(&chat, &name, picture, 28.))
            .child(div().flex_1().min_w_0().truncate().child(name))
            .when(admin, |this| {
                this.child(div().text_size(px(11.)).text_color(p.secondary).child("Admin"))
            })
    }

    fn render_sheet(&self, g: &Panel, sheet: &Sheet, p: Palette, cx: &mut Context<Self>) -> Div {
        let (title, width, primary, ready, body) = match sheet {
            Sheet::Details { name, description } => {
                let ready = !name.read(cx).value().trim().is_empty();
                let body = v_flex()
                    .gap(px(12.))
                    .child(field(
                        Input::new(name).appearance(false).aria_label("Group name").w_full(),
                        p,
                    ))
                    .child(
                        div()
                            .px(px(8.))
                            .rounded(px(6.))
                            .bg(p.field)
                            .border_1()
                            .border_color(p.separator)
                            .child(Textarea::new(description).appearance(false).aria_label("Description")),
                    );
                ("Name and description", 360., "Save".to_string(), ready, body)
            }
            Sheet::Add { search, chosen } => {
                let no_one = "Everyone you chat with is in this group.";
                let body = self.people(&self.candidates(), chosen, search, no_one, Hush::toggle_new_member, cx);
                let primary = match chosen.len() {
                    n if n > 1 => format!("Add {n}"),
                    _ => "Add".into(),
                };
                ("Add members", 380., primary, !chosen.is_empty(), body)
            }
        };
        modal(
            card(p)
                .id("group-sheet")
                .role(Role::Dialog)
                .aria_label(title)
                .w(px(width))
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" {
                        this.close_sheet(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    label("group-sheet-title", Role::Heading, title)
                        .aria_level(1)
                        .text_size(px(17.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(body)
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(div().flex_1())
                        .child(
                            Button::new("group-sheet-cancel")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| this.close_sheet(window, cx))),
                        )
                        .child(
                            Button::new("group-sheet-primary")
                                .primary()
                                .label(primary)
                                .disabled(!ready)
                                .on_click(cx.listener(|this, _, window, cx| this.save_sheet(window, cx))),
                        ),
                )
                .focus_trap("group-sheet", &g.sheet_focus),
        )
    }
}

/// A section's title.
fn heading(id: &'static str, title: String) -> Stateful<Div> {
    label(id, Role::Heading, title)
        .aria_level(2)
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
}

/// A label and a pop-up button of its choices (the Mac app's `Picker`); another choice is made
/// with `choose`.
fn picker<T: Copy + PartialEq + 'static>(
    (id, title): (&'static str, &'static str),
    current: T,
    choices: Vec<(T, String)>,
    busy: bool,
    choose: fn(&mut Hush, T, &mut Context<Hush>),
    cx: &mut Context<Hush>,
) -> Div {
    let shown = choices.iter().find(|(c, _)| *c == current).map(|(_, l)| l.clone());
    let shown = shown.unwrap_or_default();
    let this = cx.entity().downgrade();
    h_flex().gap(px(8.)).child(div().child(title)).child(
        Button::new(id)
            .label(shown.clone())
            .dropdown_caret(true)
            .accessibility_label(format!("{title}: {shown}"))
            .disabled(busy)
            .dropdown_menu(move |mut menu, _, _| {
                for (choice, name) in &choices {
                    let (this, choice) = (this.clone(), *choice);
                    let item = PopupMenuItem::new(name.clone())
                        .checked(choice == current)
                        .disabled(busy);
                    menu = menu.item(item.on_click(move |_, _, cx| {
                        if choice != current {
                            let _ = this.update(cx, |app, cx| choose(app, choice, cx));
                        }
                    }));
                }
                menu
            }),
    )
}

/// A member's menu (its … button and right-click): Make Admin or Remove as Admin, and Remove
/// from Group….
fn member_menu(
    aci: String,
    admin: bool,
    busy: bool,
    this: WeakEntity<Hush>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static {
    move |menu, _, _| {
        let (role, remove) = ((this.clone(), aci.clone()), (this.clone(), aci.clone()));
        let role_change = if admin { "Remove as Admin" } else { "Make Admin" };
        menu.item(
            PopupMenuItem::new(role_change)
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    let change = GroupChange::SetAdmin {
                        aci: role.1.clone(),
                        admin: !admin,
                    };
                    let _ = role.0.update(cx, |app, cx| app.change_group(vec![change], cx));
                }),
        )
        .item(
            PopupMenuItem::new("Remove from Group…")
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    let _ = remove
                        .0
                        .update(cx, |app, cx| app.confirm_remove(remove.1.clone(), window, cx));
                }),
        )
    }
}
