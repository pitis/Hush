//! What a message's actions draw (the Mac app's `MessageRow` and `ComposerView` sizes): the
//! buttons beside a bubble on hover, its menu, its quote, its reactions, the reply or edit bar
//! above the composer, and message text that can be selected.

use std::ops::Range;
use std::panic::Location;
use std::rc::Rc;

use gpui_kit::base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::MoveUp;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::{h_flex, v_flex, Icon, Selectable as SelectableTrigger};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::Message;

use super::{Item, RowActions, QUICK_REACTIONS};
use crate::app::view::icon;
use crate::app::Hush;
use crate::style::Palette;

/// The row the pointer is over: its buttons show.
const GROUP: &str = "message";

/// What a row's actions act on.
#[derive(Clone)]
pub(in crate::app) struct ActionCx {
    app: WeakEntity<Hush>,
    focus: FocusHandle,
}

impl Hush {
    pub(in crate::app) fn action_cx(&self, cx: &Context<Self>) -> ActionCx {
        ActionCx {
            app: cx.entity().downgrade(),
            focus: self.actions.focus().clone(),
        }
    }

    /// The composer, under the reply or the edit (with ✕, or Esc, to drop it); ↑ in its empty
    /// field edits our last message, as in Desktop.
    pub(in crate::app) fn with_composer_bar(&self, composer: Div, p: Palette, cx: &mut Context<Self>) -> Div {
        let bar = self.composer_bar().map(|(symbol, title, detail, close)| {
            h_flex()
                .gap(px(8.))
                .p(px(8.))
                .px(px(12.))
                .rounded(px(16.))
                .bg(p.field)
                .child(div().w(px(18.)).flex_none().child(icon(symbol, 14., p.secondary)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.))
                        .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(p.secondary)
                                .truncate()
                                .child(detail),
                        ),
                )
                .child(
                    div()
                        .id("composer-bar-close")
                        .role(Role::Button)
                        .aria_label(close)
                        .cursor_pointer()
                        .child(icon("circle-x", 14., p.secondary))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel(window, cx);
                        })),
                )
        });
        v_flex()
            .flex_none()
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.edit_last(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .children(bar.map(|bar| div().px(px(14.)).pt(px(8.)).child(bar)))
            .child(composer)
    }
}

/// A click that calls `f` with the row's message, as it is then.
fn on(
    act: &ActionCx,
    row: &Rc<RowActions>,
    f: impl Fn(&mut Hush, &Message, &mut Window, &mut Context<Hush>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let (app, k) = (act.app.clone(), row.key.clone());
    move |_, window, cx| {
        let _ = app.update(cx, |app, cx| {
            if let Some(m) = app.message(&k) {
                f(app, &m, window, cx);
            }
        });
    }
}

fn symbol(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// The message's menu: its right-click menu, and the … button's.
fn build_menu(
    mut menu: PopupMenu,
    row: &Rc<RowActions>,
    act: &ActionCx,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for item in &row.items {
        menu = match item {
            Item::React => {
                let (row, act) = (row.clone(), act.clone());
                menu.submenu_with_icon(
                    Some(symbol("face-slightly-smiling")),
                    "React",
                    window,
                    cx,
                    move |mut menu, _, _| {
                        for emoji in QUICK_REACTIONS {
                            let label = match row.mine.as_deref() == Some(emoji) {
                                true => format!("{emoji} ✓"),
                                false => emoji.into(),
                            };
                            let react = on(&act, &row, move |app, m, _, cx| app.react(m, emoji, cx));
                            menu = menu.item(PopupMenuItem::new(label).on_click(react));
                        }
                        menu
                    },
                )
            }
            Item::Reply => menu.item(PopupMenuItem::new("Reply").icon(symbol("reply")).on_click(on(
                act,
                row,
                Hush::reply,
            ))),
            Item::Edit => menu.item(PopupMenuItem::new("Edit").icon(symbol("pencil")).on_click(on(
                act,
                row,
                Hush::start_editing,
            ))),
            Item::Copy => menu.item(PopupMenuItem::new("Copy").icon(symbol("copy")).on_click(on(
                act,
                row,
                |app, m, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(app.copy_text(m))),
            ))),
            Item::Retry => menu.item(PopupMenuItem::new("Retry").icon(symbol("rotate-cw")).on_click(on(
                act,
                row,
                |app, m, _, cx| app.retry(m, cx),
            ))),
            Item::Delete => menu.separator().item(
                PopupMenuItem::new("Delete for Everyone…")
                    .icon(symbol("trash"))
                    .on_click(on(act, row, Hush::confirm_delete)),
            ),
        };
    }
    menu
}

/// A message's line: the pointer over it shows its buttons, a right-click its menu (the whole
/// line, gaps included).
pub(in crate::app) fn with_menu(line: Div, row: &Rc<RowActions>, act: &ActionCx) -> AnyElement {
    let line = line.group(GROUP);
    if row.items.is_empty() {
        return line.into_any_element();
    }
    let (row, act) = (row.clone(), act.clone());
    line.context_menu(move |menu, window, cx| build_menu(menu, &row, &act, window, cx))
        .into_any_element()
}

/// A hover button, as the Mac app's: a circle behind it under the pointer.
fn row_button(id: &'static str, name: &str, label: &'static str, p: Palette) -> Button {
    Button::new(id)
        .ghost()
        .icon(icon(name, 15., p.secondary))
        .accessibility_label(label)
        .size(px(26.))
        .rounded_full()
}

/// The bubble, with the menu, Reply and React beside it on hover (React nearest the bubble), in
/// the room the row keeps beside it, so they don't narrow it.
pub(in crate::app) fn beside(bubble: Div, row: &Rc<RowActions>, outgoing: bool, act: &ActionCx, p: Palette) -> Div {
    let wrap = div().relative().max_w_full().child(bubble);
    if row.deleted {
        return wrap;
    }
    let react = {
        let (app, k, act) = (act.app.clone(), row.key.clone(), act.clone());
        let row = row.clone();
        // controlled: a picker whose row scrolled away and back shows again, as `picking` says
        Popover::new("react")
            .anchor(if outgoing {
                Anchor::BottomRight
            } else {
                Anchor::BottomLeft
            })
            .trigger(row_button("react-button", "face-slightly-smiling", "React", p))
            .open(row.picking)
            .on_open_change(move |open, _, cx| {
                let _ = app.update(cx, |app, cx| app.set_picking(&k, *open, cx));
            })
            .content(move |_, _, cx| picker(&row, &act, cx.entity(), p))
    };
    let reply = row_button("reply-button", "reply", "Reply", p).on_click(on(act, row, Hush::reply));
    let more = {
        let (row, act) = (row.clone(), act.clone());
        row_button("more-button", "ellipsis", "More", p)
            .dropdown_menu(move |menu, window, cx| build_menu(menu, &row, &act, window, cx))
    };
    let buttons = h_flex()
        .absolute()
        .top_0()
        .bottom_0()
        .items_center()
        .invisible()
        .group_hover(GROUP, |s| s.visible())
        .when(row.picking, |this| this.visible());
    let buttons = match outgoing {
        true => buttons
            .right(relative(1.))
            .pr(px(2.))
            .child(more)
            .child(reply)
            .child(react),
        false => buttons
            .left(relative(1.))
            .pl(px(2.))
            .child(react)
            .child(reply)
            .child(more),
    };
    wrap.child(buttons)
}

/// Signal's reactions; ours is marked, and choosing it again takes it back.
fn picker(row: &Rc<RowActions>, act: &ActionCx, popover: Entity<PopoverState>, p: Palette) -> Div {
    h_flex()
        .gap(px(4.))
        .children(QUICK_REACTIONS.into_iter().enumerate().map(|(i, emoji)| {
            let ours = row.mine.as_deref() == Some(emoji);
            let react = on(act, row, move |app, m, _, cx| app.react(m, emoji, cx));
            let popover = popover.clone();
            div()
                .id(("reaction", i))
                .role(Role::Button)
                .aria_label(if ours {
                    format!("{emoji}, yours: remove")
                } else {
                    emoji.into()
                })
                .p(px(4.))
                .rounded_full()
                .cursor_pointer()
                .text_size(px(22.))
                .when(ours, |this| this.bg(p.bubble_out.opacity(0.5)))
                .child(emoji)
                .on_click(move |e, window, cx| {
                    react(e, window, cx);
                    popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                })
        }))
}

/// What a reply answers, above the bubble's text, under a small header.
pub(in crate::app) fn quote(row: &RowActions) -> Option<Div> {
    let (header, text, mentions) = row.quote.clone()?;
    let bold = HighlightStyle {
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    Some(
        v_flex()
            .w_full()
            .gap(px(1.))
            .p(px(8.))
            .rounded(px(10.))
            .bg(black().opacity(0.18))
            .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).child(header))
            .child(
                div()
                    .text_size(px(12.))
                    .line_clamp(2)
                    .child(StyledText::new(text).with_highlights(mentions.into_iter().map(|r| (r, bold)))),
            ),
    )
}

/// A failed send's tick: Retry.
pub(in crate::app) fn retry(row: &Rc<RowActions>, act: &ActionCx, p: Palette) -> AnyElement {
    h_flex()
        .id("retry")
        .role(Role::Button)
        .aria_label("Not sent. Retry")
        .gap(px(3.))
        .cursor_pointer()
        .text_color(p.warning)
        .child(icon("triangle-alert", 10., p.warning))
        .child("Retry")
        .on_click(on(act, row, |app, m, _, cx| app.retry(m, cx)))
        .into_any_element()
}

/// Our own look as a popover's trigger.
struct Trigger(Stateful<Div>);

impl SelectableTrigger for Trigger {
    fn selected(self, _: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl IntoElement for Trigger {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        self.0
    }
}

/// Each emoji with how many, ours tinted, half over the bubble's edge. A click shows who reacted.
pub(in crate::app) fn pill(row: &Rc<RowActions>, outgoing: bool, act: &ActionCx, p: Palette) -> Option<Div> {
    if row.pill.is_empty() {
        return None;
    }
    let spoken: Vec<String> = row.pill.iter().map(|e| format!("{} {}", e.emoji, e.count)).collect();
    let entries = row.pill.iter().map(|e| {
        let shown = match e.count {
            1 => e.emoji.clone(),
            n => format!("{} {n}", e.emoji),
        };
        div()
            .text_size(px(12.))
            .when(e.mine, |this| {
                this.px(px(4.)).rounded_full().bg(p.bubble_out.opacity(0.45))
            })
            .child(shown)
    });
    let trigger = h_flex()
        .id("pill")
        .role(Role::Button)
        .aria_label(spoken.join(", "))
        .gap(px(4.))
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .bg(p.field)
        .border_2()
        .border_color(p.window)
        .cursor_pointer()
        .children(entries);
    let (row, act) = (row.clone(), act.clone());
    let popover = Popover::new("reactors")
        .trigger(Trigger(trigger))
        .content(move |_, _, cx| reactors(&row, &act, cx.entity()));
    // as wide as its emoji, on the bubble's side
    Some(
        h_flex()
            .when(outgoing, |this| this.justify_end())
            .mt(px(-8.))
            .px(px(6.))
            .child(popover),
    )
}

/// Who reacted with what; ours can be taken back.
fn reactors(row: &Rc<RowActions>, act: &ActionCx, popover: Entity<PopoverState>) -> Div {
    v_flex()
        .min_w(px(200.))
        .gap(px(6.))
        .children(row.reactors.iter().enumerate().map(|(i, (emoji, name, ours))| {
            let line = h_flex()
                .gap(px(8.))
                .child(emoji.clone())
                .child(div().flex_1().child(name.clone()));
            if !ours {
                return line;
            }
            let (popover, emoji) = (popover.clone(), emoji.clone());
            // always a take-back, whatever was just sent: the list shows what's loaded
            let remove = on(act, row, move |app, m, _, cx| app.send_reaction(m, &emoji, true, cx));
            line.child(
                Button::new(("remove", i))
                    .label("Remove")
                    .on_click(move |e, window, cx| {
                        remove(e, window, cx);
                        popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                    }),
            )
        }))
}

/// Message text that can be selected and copied (⌘C / Ctrl+C), as `gpui_base::SelectableText`,
/// with highlights (its "@name"s in bold), which that one can't draw.
pub(in crate::app) struct Selectable {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    /// Takes the keyboard when a drag starts in it.
    focus: FocusHandle,
}

impl Selectable {
    pub(in crate::app) fn new(id: impl Into<ElementId>, text: impl Into<SharedString>, act: &ActionCx) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            styled: StyledText::new(text.clone()),
            text,
            focus: act.focus.clone(),
        }
    }

    pub(in crate::app) fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.styled = self.styled.with_highlights(highlights);
        self
    }
}

impl IntoElement for Selectable {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Selectable {
    type RequestLayoutState = TextSelectionHandle;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let id = global_id.expect("a selectable text has an id");
        let handle = window.with_element_state(id, |kept: Option<TextSelectionHandle>, _| {
            let handle = kept.unwrap_or_else(|| {
                let handle = TextSelectionHandle::new(self.text.clone(), cx);
                let focus = self.focus.clone();
                handle.focus_with(move |window, cx| focus.focus(window, cx), cx);
                handle
            });
            (handle.clone(), handle)
        });
        let (layout, ()) = self.styled.request_layout(global_id, inspector_id, window, cx);
        (layout, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox, bounds)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(handle, window, cx);
        handle.register(registration, window, cx);
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let projection = handle.update_runs(&[TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)], cx);
        let color = gpui_kit::base::Theme::global(cx).tokens.colors.selection;
        for range in projection.ranges().iter().flatten() {
            let (Some(start), Some(end)) = (
                layout.position_for_index(range.start),
                layout.position_for_index(range.end),
            ) else {
                continue;
            };
            for quad in selection_quads(start, end, bounds, layout.line_height()) {
                window.paint_quad(fill(quad, color));
            }
        }
        self.styled
            .paint(global_id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// The selection between two caret positions: the rest of the first line, the lines between,
/// and the start of the last.
fn selection_quads(start: Point<Pixels>, end: Point<Pixels>, b: Bounds<Pixels>, line: Pixels) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(start, point(end.x, end.y + line))];
    }
    let mut quads = vec![Bounds::from_corners(start, point(b.right(), start.y + line))];
    if end.y > start.y + line {
        quads.push(Bounds::from_corners(
            point(b.left(), start.y + line),
            point(b.right(), end.y),
        ));
    }
    quads.push(Bounds::from_corners(point(b.left(), end.y), point(end.x, end.y + line)));
    quads
}
