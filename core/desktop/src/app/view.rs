//! What the window draws: the Mac app's components (`DESIGN.md`, its views' sizes), in GPUI.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, Disableable, Icon, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{Conversation, ConversationKind, MessageStatus};
use sig_core::{ConnectionState, HistoryStage};

use super::{join_step, Dialog, DialogKind, Hush, JoinStep, Screen};
use crate::style::{self, Palette};
use crate::{qr, text};

const SIDEBAR: f32 = 320.;
/// The toolbar's height; on macOS the sidebar's top holds the window buttons.
const TOOLBAR: f32 = 52.;
/// Consecutive messages of one author join a run when this close (`Timeline.runGap`).
const RUN_GAP_MS: i64 = 3 * 60_000;

/// A message as the timeline shows it.
struct Row {
    /// A day separator above it ("Today").
    day: Option<String>,
    outgoing: bool,
    /// The first of a run: more space above it.
    first: bool,
    /// Above the first of a run in a group: the sender's name, in their colour.
    sender: Option<(String, Hsla)>,
    gutter: Gutter,
    text: String,
    /// The "@name"s in `text`, drawn bold.
    mentions: Vec<std::ops::Range<usize>>,
    /// Deleted: said in italics.
    note: bool,
    edited: bool,
    time: String,
    status: Option<MessageStatus>,
    /// What assistive technology reads: who, what, when.
    label: String,
}

/// The space beside incoming messages in a group: the sender's picture by the last of a run.
enum Gutter {
    None,
    Blank,
    Avatar { id: String, name: String },
}

impl Hush {
    fn rows(&self, c: &Conversation) -> Vec<Row> {
        let group = c.kind == ConversationKind::Group;
        let now = chrono::Local::now();
        let ms = &self.messages;
        let joins = |a: &sig_core::projection::Message, b: &sig_core::projection::Message| {
            a.author == b.author
                && a.is_outgoing == b.is_outgoing
                && (b.sent_at - a.sent_at).abs() < RUN_GAP_MS
                && text::same_day(a.sent_at, b.sent_at)
        };
        let rows = ms.iter().enumerate().map(|(i, m)| {
            let prev = i.checked_sub(1).map(|j| &ms[j]);
            let next = ms.get(i + 1);
            let new_day = prev.is_none_or(|p| !text::same_day(p.sent_at, m.sent_at));
            let first = new_day || !prev.is_some_and(|p| joins(p, m));
            let last = !next.is_some_and(|n| joins(m, n));
            let name = self.name(&m.author);
            let person = format!("c:{}", m.author);
            let (body, mentions) = text::message(m, |aci| self.name(aci));
            let time = text::bubble_time(m.sent_at);
            let status = m.is_outgoing.then_some(m.status);
            let said = status.map(|s| format!(", {}", text::status(s))).unwrap_or_default();
            Row {
                day: new_day.then(|| text::day(m.sent_at, now)),
                outgoing: m.is_outgoing,
                first,
                sender: (group && !m.is_outgoing && first).then(|| (name.clone(), style::avatar_color(&person))),
                gutter: match () {
                    _ if !group || m.is_outgoing => Gutter::None,
                    _ if last => Gutter::Avatar {
                        id: person,
                        name: name.clone(),
                    },
                    _ => Gutter::Blank,
                },
                label: format!("{name}: {body}. {time}{said}"),
                text: body,
                mentions,
                note: m.deleted,
                edited: m.edited && !m.deleted,
                time,
                status,
            }
        });
        rows.collect()
    }

    fn render_link(&self, url: Option<&str>, p: Palette, cx: &mut Context<Self>) -> Div {
        let failed = url.is_none() && self.error.is_some();
        let code = match url {
            Some(url) => qr::code(url, 280.).map(IntoElement::into_any_element),
            None if failed => Some(icon("triangle-alert", 26., p.secondary).into_any_element()),
            None => Some(Spinner::new().into_any_element()),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(18.))
            .p(px(40.))
            .child(
                label("link-title", Role::Heading, "Link Hush to your phone")
                    .aria_level(1)
                    .text_size(px(17.))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                label(
                    "link-how",
                    Role::Label,
                    "On your phone: Settings → Linked devices → Link new device, then scan this code.",
                )
                .max_w(px(360.))
                .text_center()
                .text_color(p.secondary),
            )
            .child(
                label(
                    "link-history",
                    Role::Label,
                    "If your phone offers to transfer your message history, accept it to see your older messages here.",
                )
                .max_w(px(360.))
                .text_center()
                .text_size(px(12.))
                .text_color(p.secondary),
            )
            .child(
                div()
                    .id("link-code")
                    .when(url.is_some(), |this| {
                        this.role(Role::Image).aria_label("Linking QR code")
                    })
                    .size(px(280.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(code),
            )
            .when_some(self.error.clone().filter(|_| failed), |this, error| {
                this.child(
                    label("link-error", Role::Alert, error)
                        .max_w(px(360.))
                        .text_center()
                        .text_size(px(12.))
                        .text_color(p.danger),
                )
                .child(
                    Button::new("retry")
                        .primary()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_link(cx))),
                )
            })
    }

    fn render_sidebar(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let shown: Vec<&Conversation> = self.chats.iter().filter(|c| self.matches(c)).collect();
        let pinned = self.query.is_empty() && shown.iter().any(|c| c.pinned_order.is_some());
        let mut list = v_flex().pb(px(8.));
        for (ix, c) in shown.iter().enumerate() {
            let first_unpinned = c.pinned_order.is_none() && ix > 0 && shown[ix - 1].pinned_order.is_some();
            if pinned && ix == 0 {
                list = list.child(section("Pinned", p));
            }
            if pinned && first_unpinned {
                list = list.child(section("Chats", p));
            }
            list = list.child(self.render_chat_row(ix, c, p, cx));
        }
        if shown.is_empty() && !self.query.is_empty() {
            list = list.child(
                label("no-results", Role::Label, "No chats found")
                    .px(px(20.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .text_color(p.secondary),
            );
        }
        // macOS 26's sidebar: a panel floating in the window, inset from its edges
        v_flex()
            .w(px(SIDEBAR))
            .flex_none()
            .m(px(8.))
            .mr_0()
            .rounded(px(16.))
            .overflow_hidden()
            .bg(p.sidebar)
            .border_1()
            .border_color(p.separator)
            .when(cfg!(target_os = "macos"), |this| {
                this.child(
                    div()
                        .h(px(TOOLBAR - 8.))
                        .flex_none()
                        .window_control_area(WindowControlArea::Drag),
                )
            })
            .child(
                h_flex()
                    .px(px(10.))
                    .pb(px(8.))
                    .pt(px(8.))
                    .gap(px(6.))
                    .flex_none()
                    .child(
                        h_flex()
                            .flex_1()
                            .h(px(30.))
                            .px(px(8.))
                            .gap(px(6.))
                            .rounded(px(8.))
                            .bg(p.fill)
                            .child(icon("search", 14., p.secondary))
                            .child(
                                Input::new(&self.search)
                                    .appearance(false)
                                    .aria_label("Search")
                                    .text_size(px(13.))
                                    .flex_1(),
                            ),
                    )
                    .child(self.render_new_chat(p, cx)),
            )
            .children(self.error.clone().map(|e| self.render_error(e, p, cx)))
            .child(
                div()
                    .id("chat-list")
                    .role(Role::List)
                    .aria_label("Chats")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .children(self.render_connection(p, cx))
    }

    /// The Mac app's New Chat menu (`SidebarView.swift`).
    fn render_new_chat(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        Button::new("new-chat")
            .ghost()
            .icon(icon("square-pen", 15., p.secondary))
            .accessibility_label("New Chat")
            .tooltip("New chat")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let item = |label: &'static str, kind: fn() -> DialogKind| {
                    let this = this.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let _ = this.update(cx, |app, cx| app.open_dialog(kind(), window, cx));
                    })
                };
                menu.item(item("New Chat by Username…", || DialogKind::Username))
                    .item(item("New Group…", || DialogKind::Group(Default::default())))
                    .item(item("Join Group by Link…", || DialogKind::Join(None)))
            })
    }

    /// The open New Chat dialog over the window (the Mac app's sheets).
    fn render_dialog(&self, d: &Dialog, p: Palette, cx: &mut Context<Self>) -> Div {
        let asked = !d.field.read(cx).value().trim().is_empty();
        let (title, detail) = match &d.kind {
            DialogKind::Username => ("New chat", Some("Enter their username, as it shows in their profile.")),
            DialogKind::Group(_) => ("New group", None),
            DialogKind::Join(_) => ("Join a group", None),
        };
        let primary = match &d.kind {
            DialogKind::Username => "Find",
            DialogKind::Group(_) => "Create",
            DialogKind::Join(None) => "Show Group",
            DialogKind::Join(Some(g)) => match join_step(g, self.chats.iter().any(|c| c.id == g.conversation_id)) {
                JoinStep::Open => "Open Chat",
                JoinStep::Join => "Join",
                JoinStep::AskToJoin => "Ask to Join",
            },
        };
        let body = match &d.kind {
            DialogKind::Group(_) if self.contacts.is_empty() => Some(
                label("no-one", Role::Label, "No one to add yet; members can be added later.")
                    .text_size(px(12.))
                    .text_color(p.secondary)
                    .into_any_element(),
            ),
            DialogKind::Group(chosen) => {
                let people = self.contacts.iter().enumerate().map(|(ix, (aci, name))| {
                    let aci = aci.clone();
                    let this = cx.entity().downgrade();
                    Checkbox::new(("member", ix))
                        .label(name.clone())
                        .checked(chosen.contains(&aci))
                        .on_click(move |_, _, cx| {
                            let _ = this.update(cx, |app, cx| app.toggle_member(aci.clone(), cx));
                        })
                });
                Some(
                    v_flex()
                        .id("members")
                        .role(Role::List)
                        .aria_label("Members")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .gap(px(6.))
                        .children(people)
                        .into_any_element(),
                )
            }
            DialogKind::Join(Some(g)) => {
                let members = match g.member_count {
                    1 => "1 member".to_string(),
                    n => format!("{n} members"),
                };
                let standing = match () {
                    _ if g.member => "You're in this group.",
                    _ if g.requested => "You asked to join: an admin has to let you in.",
                    _ if g.approval => "An admin has to approve you.",
                    _ => "",
                };
                Some(
                    v_flex()
                        .gap(px(4.))
                        .child(
                            label(
                                "group-title",
                                Role::Label,
                                if g.title.is_empty() {
                                    "Unnamed group".into()
                                } else {
                                    g.title.clone()
                                },
                            )
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD),
                        )
                        .when(!g.description.is_empty(), |this| {
                            this.child(
                                label("group-description", Role::Label, g.description.clone())
                                    .text_size(px(12.))
                                    .text_color(p.secondary),
                            )
                        })
                        .child(label("group-members", Role::Label, members).text_size(px(12.)))
                        .when(!standing.is_empty(), |this| {
                            this.child(
                                label("group-standing", Role::Label, standing)
                                    .text_size(px(12.))
                                    .text_color(p.secondary),
                            )
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        };
        // the chats behind don't take clicks while it's open
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(black().opacity(0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("dialog")
                    .role(Role::Dialog)
                    .aria_label(title)
                    .w(px(380.))
                    .p(px(20.))
                    .gap(px(12.))
                    .rounded(px(12.))
                    .bg(p.window)
                    .border_1()
                    .border_color(p.separator)
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                        if e.keystroke.key == "escape" {
                            this.close_dialog(cx);
                        }
                    }))
                    .child(
                        label("dialog-title", Role::Heading, title)
                            .aria_level(1)
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .children(detail.map(|d| {
                        label("dialog-detail", Role::Label, d)
                            .text_size(px(12.))
                            .text_color(p.secondary)
                    }))
                    .child(
                        div()
                            .h(px(28.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(p.field)
                            .border_1()
                            .border_color(p.separator)
                            .child(Input::new(&d.field).appearance(false).aria_label(title).w_full()),
                    )
                    .children(body)
                    .children(d.problem.clone().map(|why| {
                        label("dialog-problem", Role::Alert, why)
                            .text_size(px(12.))
                            .text_color(p.danger)
                    }))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .when(d.busy, |this| this.child(Spinner::new().xsmall()))
                            .child(div().flex_1())
                            .child(
                                Button::new("dialog-cancel")
                                    .label("Cancel")
                                    .disabled(d.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                            )
                            .child(
                                Button::new("dialog-primary")
                                    .primary()
                                    .label(primary)
                                    .disabled(d.busy || !asked)
                                    .on_click(cx.listener(|this, _, window, cx| this.submit_dialog(window, cx))),
                            ),
                    ),
            )
    }

    fn matches(&self, c: &Conversation) -> bool {
        let q = &self.query;
        q.is_empty()
            || text::title(c, self.own_aci.as_deref()).to_lowercase().contains(q)
            || c.last_message_preview.to_lowercase().contains(q)
    }

    fn render_chat_row(&self, ix: usize, c: &Conversation, p: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let title = text::title(c, self.own_aci.as_deref());
        let line = text::preview(c, &self.name(&c.last_message_author), |aci| self.name(aci));
        let selected = self.selected.as_deref() == Some(c.id.as_str());
        // the system's sidebar: the accent while the window is active, else grey
        let focused = selected && self.window_active;
        let (fg, secondary) = if focused {
            (p.on_accent, p.on_accent.opacity(0.8))
        } else {
            (p.text, p.secondary)
        };
        let unread = c.unread_count > 0 || c.marked_unread;
        let muted = c.mute_until > chrono::Utc::now().timestamp_millis();
        let tick = (c.last_message_outgoing && c.last_message_at > 0 && !c.last_message_deleted).then(|| {
            let behind = if focused {
                p.accent
            } else if selected {
                p.selection
            } else {
                p.sidebar
            };
            tick(
                c.last_message_status,
                12.,
                if unread { fg } else { secondary },
                behind,
                p,
            )
        });
        let badge = unread.then(|| {
            let (bg, color) = if focused {
                (p.on_accent, p.accent)
            } else {
                (p.accent, p.on_accent)
            };
            match c.unread_count {
                0 => div().size(px(8.)).flex_none().rounded_full().bg(bg),
                n => div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded_full()
                    .bg(bg)
                    .text_color(color)
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .child(n.to_string()),
            }
        });
        let spoken = match (c.unread_count, unread) {
            (n, true) if n > 0 => format!("{title}, {n} unread. {line}"),
            (_, true) => format!("{title}, unread. {line}"),
            _ => format!("{title}. {line}"),
        };
        // a blocked chat's or a request's picture isn't shown (a stranger chose it)
        let picture = c.avatar.as_deref().filter(|_| !c.blocked && !c.message_request);
        let id = c.id.clone();
        div()
            .id(("chat", ix))
            .role(Role::ListItem)
            .aria_label(spoken)
            .aria_selected(selected)
            .px(px(10.))
            .on_click(cx.listener(move |this, _, window, cx| this.select(id.clone(), window, cx)))
            .child(
                h_flex()
                    .h(px(56.))
                    .px(px(8.))
                    .gap(px(10.))
                    .rounded(px(8.))
                    .when(selected, |this| this.bg(if focused { p.accent } else { p.selection }))
                    .child(avatar(&c.id, &title, picture, 40.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(fg)
                                            .font_weight(if unread { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
                                            .child(title),
                                    )
                                    .when(muted, |this| this.child(icon("bell-off", 10., secondary)))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(secondary)
                                            .child(text::list_time(c.last_message_at, chrono::Local::now())),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .children(tick)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(if unread { fg } else { secondary })
                                            .child(line),
                                    )
                                    .children(badge),
                            ),
                    ),
            )
    }

    fn render_error(&self, error: String, p: Palette, cx: &mut Context<Self>) -> Div {
        div().px(px(10.)).pb(px(6.)).flex_none().child(
            h_flex()
                .items_start()
                .gap(px(8.))
                .p(px(10.))
                .rounded(px(10.))
                .bg(p.fill)
                .child(icon("triangle-alert", 13., p.warning))
                .child(
                    label("error", Role::Alert, error)
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .line_clamp(3),
                )
                .child(
                    div()
                        .id("error-close")
                        .role(Role::Button)
                        .aria_label("Dismiss")
                        .cursor_pointer()
                        .child(icon("x", 12., p.secondary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                ),
        )
    }

    /// One quiet line at the sidebar's foot; nothing while online.
    fn render_connection(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let line = h_flex()
            .flex_none()
            .gap(px(6.))
            .px(px(16.))
            .py(px(8.))
            .text_size(px(12.))
            .text_color(p.secondary);
        match self.connection {
            ConnectionState::Online | ConnectionState::Unlinked => None,
            ConnectionState::Connecting => Some(line.child(Spinner::new().xsmall()).child(label(
                "connection",
                Role::Status,
                "Connecting…",
            ))),
            ConnectionState::Offline => Some(
                line.child(icon("wifi-off", 12., p.secondary))
                    .child(label("connection", Role::Status, "Offline").flex_1())
                    .child(
                        div()
                            .id("reconnect")
                            .role(Role::Button)
                            .aria_label("Retry")
                            .cursor_pointer()
                            .text_color(p.accent)
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                    ),
            ),
        }
    }

    fn render_chat(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let open = self
            .selected
            .as_ref()
            .and_then(|id| self.chats.iter().find(|c| &c.id == id));
        let Some(c) = open else {
            let (title, detail) = match (self.chats.is_empty(), &self.connection) {
                (false, _) => ("Select a chat", ""),
                (true, ConnectionState::Online) => (
                    "Connected — no messages yet",
                    "New messages appear here as they arrive.",
                ),
                (true, _) => ("Connecting…", "New messages appear here as they arrive."),
            };
            return v_flex()
                .flex_1()
                .h_full()
                .child(
                    div()
                        .h(px(TOOLBAR))
                        .flex_none()
                        .window_control_area(WindowControlArea::Drag),
                )
                .child(empty_state("messages-square", title, detail, p));
        };
        let rows = Rc::new(self.rows(c));
        let out_fill = style::bubble_fill(c.chat_color.as_ref(), p.bubble_out);
        let title = text::title(c, self.own_aci.as_deref());
        let subtitle = match &self.connection {
            ConnectionState::Offline => "Offline".to_string(),
            ConnectionState::Connecting => "Connecting…".to_string(),
            _ if c.expire_timer > 0 => format!("Disappearing messages: {}", text::duration(c.expire_timer)),
            _ => match (&c.kind, &c.username) {
                (ConversationKind::Direct, Some(u)) if !c.title.is_empty() => format!("@{u}"),
                _ => String::new(),
            },
        };
        // shortcut: requests, invitations, join requests and blocked chats are answered on the
        // phone; the Mac app offers Accept, Block and the others here
        let answer_first = c.message_request || c.group_invitation || c.group_join_requested || c.blocked;
        let answer_where = if c.blocked {
            "This chat is blocked. Unblock it on your phone to write here."
        } else if c.group_join_requested {
            "You asked to join this group. An admin has to let you in."
        } else {
            "Answer this chat on your phone to write here."
        };
        let mut row_style = StyleRefinement::default();
        row_style.padding.bottom = Some(px(0.).into());
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(p.window)
            .child(
                v_flex()
                    .h(px(TOOLBAR))
                    .flex_none()
                    .justify_center()
                    .px(px(16.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        label("chat-title", Role::Heading, title)
                            .aria_level(2)
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .truncate(),
                    )
                    .when(!subtitle.is_empty(), |this| {
                        this.child(
                            label("chat-subtitle", Role::Label, subtitle)
                                .text_size(px(11.))
                                .text_color(p.secondary),
                        )
                    }),
            )
            .child(
                MessageScroller::new("timeline", self.timeline.clone(), move |ix, _, cx| {
                    render_row(ix, &rows[ix], &out_fill, style::palette(cx))
                })
                .with_row_style(row_style)
                .flex_1()
                .min_h_0(),
            )
            .child(if answer_first {
                let id = c.id.clone();
                v_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(18.))
                    .py(px(14.))
                    .child(
                        label("answer-first", Role::Label, answer_where)
                            .text_center()
                            .text_color(p.secondary),
                    )
                    .when(c.group_join_requested && !c.blocked, |this| {
                        this.child(
                            Button::new("cancel-request")
                                .label("Cancel Request")
                                .disabled(self.cancelling)
                                .on_click(cx.listener(move |this, _, _, cx| this.cancel_join_request(id.clone(), cx))),
                        )
                    })
            } else {
                self.render_composer(p, cx)
            })
    }

    fn render_composer(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        div().flex_none().px(px(14.)).pt(px(8.)).pb(px(12.)).child(
            h_flex()
                .items_end()
                .min_h(px(35.))
                .rounded(px(18.))
                .bg(p.field)
                // with the field's own 10×8 padding: the Mac app's 14×9
                .pl(px(4.))
                .pr(px(if self.composer_empty { 4. } else { 6. }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .py(px(1.))
                        .text_size(px(14.))
                        .child(Textarea::new(&self.composer).appearance(false).aria_label("Message")),
                )
                .when(!self.composer_empty, |this| {
                    this.child(
                        div()
                            .id("send")
                            .role(Role::Button)
                            .aria_label("Send")
                            .h(px(35.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .child(
                                div()
                                    .size(px(24.))
                                    .rounded_full()
                                    .bg(p.accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icon("arrow-up", 15., p.on_accent)),
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
                }),
        )
    }

    /// The history transfer's progress over the chats, with a way out at every stage (the Mac
    /// app's `HistoryView.swift`).
    fn render_history(
        &self,
        (stage, done, total): (HistoryStage, u64, u64),
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let failed = stage == HistoryStage::Failed;
        let title = match stage {
            HistoryStage::Waiting => "Preparing to transfer…",
            HistoryStage::Downloading => "Transferring your messages…",
            HistoryStage::Importing => "Importing your messages…",
            HistoryStage::Failed => "The transfer didn't finish",
            HistoryStage::Done | HistoryStage::NoHistory => "",
        };
        let detail = match stage {
            HistoryStage::Waiting => Some("Keep the app open on your phone.".to_string()),
            HistoryStage::Downloading if total > 0 => Some(format!(
                "{:.0} of {:.0} MB",
                done as f64 / 1_048_576.,
                total as f64 / 1_048_576.
            )),
            HistoryStage::Importing if done > 0 => Some(format!("{done} items")),
            HistoryStage::Failed => Some("Your chats keep working; new messages arrive once you continue.".into()),
            _ => None,
        };
        let progress = match stage {
            HistoryStage::Failed => icon("triangle-alert", 26., p.secondary).into_any_element(),
            HistoryStage::Downloading if total > 0 => div()
                .w(px(220.))
                .h(px(4.))
                .rounded_full()
                .bg(p.fill)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(p.accent)
                        .w(relative((done as f32 / total as f32).clamp(0., 1.))),
                )
                .into_any_element(),
            _ => Spinner::new().into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .bg(black().opacity(0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .max_w(px(380.))
                    .p(px(28.))
                    .gap(px(14.))
                    .items_center()
                    .rounded(px(12.))
                    .bg(p.window)
                    .border_1()
                    .border_color(p.separator)
                    .child(progress)
                    .child(label("history-title", Role::Status, title).font_weight(FontWeight::BOLD))
                    .children(detail.map(|d| {
                        label("history-detail", Role::Label, d)
                            .text_size(px(12.))
                            .text_color(p.secondary)
                            .text_center()
                    }))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("history-continue")
                                    .label("Continue Without History")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_history(cx))),
                            )
                            .when(failed, |this| {
                                this.child(
                                    Button::new("history-retry")
                                        .primary()
                                        .label("Retry")
                                        .on_click(cx.listener(|this, _, _, cx| this.retry_history(cx))),
                                )
                            }),
                    ),
            )
    }
}

fn render_row(ix: usize, row: &Row, out_fill: &Background, p: Palette) -> Stateful<Div> {
    let (fill, color) = if row.outgoing {
        (*out_fill, p.on_accent)
    } else {
        (p.bubble_in.into(), p.text_in)
    };
    let meta_color = color.opacity(0.6);
    let meta = h_flex()
        .flex_none()
        .gap(px(3.))
        .pb(px(1.))
        .text_size(px(10.))
        .text_color(meta_color)
        .when(row.edited, |this| this.child("edited"))
        .child(row.time.clone())
        .children(row.status.map(|s| tick(s, 10., meta_color, p.bubble_out, p)));
    let bubble = h_flex()
        .max_w_full()
        .items_end()
        .gap(px(8.))
        .px(px(12.))
        .py(px(8.))
        .rounded(px(18.))
        .bg(fill)
        .text_color(color)
        .child(
            div()
                .min_w_0()
                .text_size(px(14.))
                .when(row.note, |this| this.italic().text_color(color.opacity(0.6)))
                .child(
                    StyledText::new(row.text.clone()).with_highlights(row.mentions.iter().map(|r| {
                        let bold = HighlightStyle {
                            font_weight: Some(FontWeight::BOLD),
                            ..Default::default()
                        };
                        (r.clone(), bold)
                    })),
                ),
        )
        .child(meta);
    let gutter = match &row.gutter {
        Gutter::None => None,
        Gutter::Blank => Some(div().size(px(28.)).flex_none()),
        Gutter::Avatar { id, name } => Some(avatar(id, name, None, 28.)),
    };
    v_flex()
        .id(ix)
        .role(Role::ListItem)
        .aria_label(row.label.clone())
        .w_full()
        .px(px(16.))
        .children(row.day.clone().map(|day| {
            div()
                .w_full()
                .pt(px(18.))
                .pb(px(2.))
                .flex()
                .justify_center()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.secondary)
                .child(day)
        }))
        .child(
            h_flex()
                .w_full()
                .items_end()
                .gap(px(8.))
                .pt(px(if row.first { 8. } else { 2. }))
                .map(|this| {
                    if row.outgoing {
                        this.justify_end().pl(px(80.))
                    } else {
                        this.pr(px(80.))
                    }
                })
                .children(gutter)
                .child(
                    v_flex()
                        .max_w(px(560.))
                        .min_w_0()
                        .gap(px(2.))
                        .when(row.outgoing, |this| this.items_end())
                        .children(row.sender.clone().map(|(name, color)| {
                            div()
                                .px(px(12.))
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color)
                                .child(name)
                        }))
                        .child(bubble),
                ),
        )
}

/// A person's or chat's picture, or their initials on their colour.
fn avatar(id: &str, name: &str, picture: Option<&str>, size: f32) -> Div {
    let base = style::avatar_color(id);
    let top = Hsla {
        l: (base.l + 0.08).min(1.),
        ..base
    };
    let circle = div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center();
    match picture {
        Some(path) => circle.child(img(PathBuf::from(path)).size_full().object_fit(ObjectFit::Cover)),
        None => circle
            .bg(linear_gradient(
                180.,
                linear_color_stop(top, 0.),
                linear_color_stop(base, 1.),
            ))
            .text_color(white())
            .text_size(px(size * 0.38))
            .font_weight(FontWeight::SEMIBOLD)
            .child(style::initials(name)),
    }
}

/// Delivery as the Mac app draws it: a clock, a check, a check in a circle, a filled circle
/// (its check cut out of `behind`, the colour it sits on).
fn tick(status: MessageStatus, size: f32, color: Hsla, behind: Hsla, p: Palette) -> AnyElement {
    match status {
        MessageStatus::Pending => icon("clock", size, color).into_any_element(),
        MessageStatus::Sent => icon("check", size, color).into_any_element(),
        MessageStatus::Delivered => icon("circle-check", size, color).into_any_element(),
        MessageStatus::Read => div()
            .size(px(size))
            .rounded_full()
            .bg(color)
            .flex()
            .items_center()
            .justify_center()
            .child(icon("check", size * 0.7, behind))
            .into_any_element(),
        MessageStatus::Failed => h_flex()
            .gap(px(3.))
            .text_color(p.warning)
            .child(icon("triangle-alert", size, p.warning))
            .child("Not sent")
            .into_any_element(),
    }
}

fn icon(name: &str, size: f32, color: Hsla) -> Icon {
    Icon::empty()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
        .text_color(color)
}

fn section(title: &'static str, p: Palette) -> Div {
    div()
        .px(px(20.))
        .pt(px(10.))
        .pb(px(4.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.secondary)
        .child(title)
}

fn empty_state(symbol: &str, title: &'static str, detail: impl Into<SharedString>, p: Palette) -> Div {
    let detail = detail.into();
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .p(px(40.))
        .child(icon(symbol, 44., p.secondary))
        .child(
            label("empty-title", Role::Heading, title)
                .aria_level(1)
                .text_size(px(17.))
                .font_weight(FontWeight::BOLD),
        )
        .when(!detail.is_empty(), |this| {
            this.child(
                label("empty-detail", Role::Label, detail)
                    .max_w(px(360.))
                    .text_center()
                    .text_color(p.secondary),
            )
        })
}

/// Text that assistive technology reads too: GPUI shows it only elements with an id and a role;
/// the label is what it reads.
fn label(id: impl Into<ElementId>, role: Role, text: impl Into<SharedString>) -> Stateful<Div> {
    let text = text.into();
    div().id(id).role(role).aria_label(text.clone()).child(text)
}

impl Render for Hush {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = style::palette(cx);
        let screen = match &self.screen {
            Screen::Opening => div().size_full().flex().items_center().justify_center().child(
                div()
                    .id("opening")
                    .role(Role::ProgressIndicator)
                    .aria_label("Opening")
                    .child(Spinner::new()),
            ),
            Screen::Link { url } => self.render_link(url.clone().as_deref(), p, cx),
            Screen::Chats => h_flex()
                .size_full()
                .items_stretch()
                .child(self.render_sidebar(p, cx))
                .child(self.render_chat(p, cx)),
            Screen::Unlinked => v_flex().size_full().child(empty_state(
                "smartphone",
                "This computer is no longer linked",
                format!(
                    "It was removed from your phone's linked devices. Linking it again isn't in this version yet: quit Hush, delete the folder {}, and open Hush again.",
                    self.data_dir.display()
                ),
                p,
            )),
            Screen::Failed(e) => v_flex()
                .size_full()
                .child(empty_state("octagon-alert", "Hush couldn't start", e.clone(), p)),
        };
        let history = match self.screen {
            Screen::Chats => self.history.map(|h| self.render_history(h, p, cx)),
            _ => None,
        };
        let dialog = match self.screen {
            Screen::Chats => self.dialog.as_ref().map(|d| self.render_dialog(d, p, cx)),
            _ => None,
        };
        div()
            .relative()
            .size_full()
            .bg(p.window)
            .text_color(p.text)
            .text_size(px(13.))
            .child(screen)
            .children(history)
            .children(dialog)
    }
}
