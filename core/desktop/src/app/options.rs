//! The open chat's options (the Mac app's Chat Options menu, `ConversationChrome`): its
//! disappearing-message timer, and its colour and wallpaper (`ChatStylePicker`), which are this
//! computer's only.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sig_core::projection::{ChatColor, Conversation, ConversationKind};

use super::actions::view::Trigger;
use super::view::icon;
use super::Hush;
use crate::style::{self, Palette};
use crate::text;

/// The timers offered, as in Desktop (seconds; 0: off; `Placeholder.timerChoices`).
const TIMERS: [u32; 8] = [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200];

#[derive(Default)]
pub(super) struct Options {
    /// The colour and wallpaper picker is open, over the chat open then (its `generation`).
    styling: Option<u64>,
}

/// Sets one of a chat's styles (None: the default).
type SetStyle = fn(&mut Hush, String, Option<ChatColor>, &mut Context<Hush>);

impl Hush {
    /// The Chat Options menu, at the toolbar's right, and the colour and wallpaper picker, which
    /// shows under it.
    pub(super) fn render_options(&self, c: &Conversation, p: Palette, cx: &mut Context<Self>) -> Div {
        let this = cx.entity().downgrade();
        let (id, current, timer) = (c.id.clone(), c.expire_timer, can_set_timer(c));
        let menu = {
            let this = this.clone();
            Button::new("chat-options")
                .ghost()
                .icon(icon("info", 15., p.secondary))
                .accessibility_label("Chat Options")
                .tooltip("Chat options")
                .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                    let menu = match timer {
                        true => {
                            let (this, id) = (this.clone(), id.clone());
                            menu.submenu("Disappearing Messages", window, cx, move |mut menu, _, _| {
                                for seconds in timer_choices(current) {
                                    let (this, id) = (this.clone(), id.clone());
                                    let item = PopupMenuItem::new(timer_choice(seconds))
                                        .checked(seconds == current)
                                        // the one set again would be sent again, and shown to both
                                        .when(seconds != current, |item| {
                                            item.on_click(move |_, _, cx| {
                                                let _ =
                                                    this.update(cx, |app, cx| app.set_timer(id.clone(), seconds, cx));
                                            })
                                        });
                                    menu = menu.item(item);
                                }
                                menu
                            })
                        }
                        false => menu,
                    };
                    let this = this.clone();
                    menu.item(
                        PopupMenuItem::new("Chat Colour and Wallpaper…").on_click(move |_, _, cx| {
                            let _ = this.update(cx, |app, cx| {
                                app.options.styling = Some(app.generation);
                                cx.notify();
                            });
                        }),
                    )
                })
        };
        let (id, color, wallpaper) = (c.id.clone(), c.chat_color.clone(), c.wallpaper.clone());
        let picker = Popover::new("chat-style")
            .anchor(Anchor::TopRight)
            .open(self.options.styling == Some(self.generation))
            .on_open_change({
                let this = this.clone();
                move |open, _, cx| {
                    if !*open {
                        let _ = this.update(cx, |app, cx| {
                            app.options.styling = None;
                            cx.notify();
                        });
                    }
                }
            })
            // shown from the menu only: under the button, at its right edge
            .trigger(Trigger(div().id("chat-style-anchor").h(px(28.))))
            .content(move |_, _, _| {
                let section = |title, presets, current: &Option<ChatColor>, set| {
                    swatches(title, presets, current.as_ref(), set, &id, &this, p)
                };
                v_flex()
                    .gap(px(10.))
                    .child(section(
                        "Chat colour",
                        sig_core::chat_color_presets(),
                        &color,
                        Hush::set_chat_color,
                    ))
                    .child(div().h(px(1.)).bg(p.separator))
                    .child(section(
                        "Wallpaper",
                        sig_core::wallpaper_presets(),
                        &wallpaper,
                        Hush::set_wallpaper,
                    ))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(p.secondary)
                            .child("For this chat, on this computer."),
                    )
            });
        // over the toolbar's drag area, which would take its clicks
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(10.))
            .flex()
            .items_center()
            .occlude()
            .child(menu)
            .child(picker)
    }

    /// Sets a direct chat's disappearing-message timer, for both sides.
    fn set_timer(&mut self, id: String, seconds: u32, cx: &mut Context<Self>) {
        self.change("set the disappearing timer", cx, move |client| async move {
            client.set_disappearing_timer(id, seconds).await
        });
    }

    fn set_chat_color(&mut self, id: String, color: Option<ChatColor>, cx: &mut Context<Self>) {
        self.change("set the chat colour", cx, move |client| async move {
            client.set_chat_color(id, color).await
        });
    }

    fn set_wallpaper(&mut self, id: String, wallpaper: Option<ChatColor>, cx: &mut Context<Self>) {
        self.change("set the wallpaper", cx, move |client| async move {
            client.set_wallpaper(id, wallpaper).await
        });
    }

    /// `--snapshot`'s chat colour and wallpaper, with the picker open.
    #[cfg(feature = "snapshot")]
    pub(super) fn demo_style(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            c.chat_color = sig_core::chat_color_presets().into_iter().nth(15);
            c.wallpaper = sig_core::wallpaper_presets().into_iter().nth(19);
        }
        self.options.styling = Some(self.generation);
    }
}

/// A style's presets as swatches, the one chosen ringed; a click sets one, Use Default the default.
fn swatches(
    title: &'static str,
    presets: Vec<ChatColor>,
    current: Option<&ChatColor>,
    set: SetStyle,
    id: &str,
    this: &WeakEntity<Hush>,
    p: Palette,
) -> Stateful<Div> {
    let choose = |color: Option<ChatColor>| {
        let (this, id) = (this.clone(), id.to_owned());
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = this.update(cx, |app, cx| set(app, id.clone(), color.clone(), cx));
        }
    };
    let shown = presets.into_iter().enumerate().map(|(i, c)| {
        let chosen = current == Some(&c);
        let name = format!("{title} {}", i + 1);
        div()
            .id(("swatch", i))
            .role(Role::Button)
            .aria_label(name)
            .aria_selected(chosen)
            .size(px(38.))
            .rounded_full()
            .border_2()
            .border_color(if chosen { p.text } else { transparent_black() })
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .child(
                div()
                    .size(px(30.))
                    .rounded_full()
                    .bg(style::bubble_fill(Some(&c), p.bubble_out)),
            )
            .on_click(choose(Some(c)))
    });
    v_flex()
        .id(title)
        .gap(px(10.))
        .child(
            h_flex()
                .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    Button::new("use-default")
                        .small()
                        .label("Use Default")
                        .on_click(choose(None)),
                ),
        )
        // six to a row, as the Mac app's
        .child(h_flex().w(px(238.)).flex_wrap().gap(px(2.)).children(shown))
}

/// The timeline over the chat's wallpaper, if it has one.
pub(super) fn with_wallpaper(timeline: Div, c: &Conversation, cx: &App) -> Div {
    let dark = cx.theme().mode.is_dark();
    timeline.when_some(c.wallpaper.as_ref(), |t, w| t.bg(style::wallpaper_fill(w, dark)))
}

/// What sits on the timeline outside a bubble, on a backing when there's a wallpaper under it, to
/// stay readable (`wallpaperBacked`).
pub(super) fn backed(content: Div, wallpaper: bool, p: Palette) -> Div {
    if !wallpaper {
        return content;
    }
    h_flex().child(content.px(px(10.)).py(px(4.)).rounded_full().bg(p.window.opacity(0.8)))
}

/// The choices, with `current` among them when the phone set another (2 days, say).
fn timer_choices(current: u32) -> Vec<u32> {
    let mut all = TIMERS.to_vec();
    if !all.contains(&current) {
        all.push(current);
        all.sort();
    }
    all
}

fn timer_choice(seconds: u32) -> String {
    match seconds {
        0 => "Off".into(),
        s => text::duration(s),
    }
}

/// A group's timer changes with the group (from the phone); a blocked or unaccepted chat has
/// nothing to send it to.
fn can_set_timer(c: &Conversation) -> bool {
    c.kind == ConversationKind::Direct && !c.blocked && !c.message_request && !c.id.starts_with("c:PNI:")
}

#[cfg(test)]
mod tests {
    use sig_core::projection::Conversation;

    use super::super::tests::chat;
    use super::{can_set_timer, timer_choice, timer_choices};

    #[test]
    fn the_timers_offered_are_desktops_with_the_one_set_on_the_phone() {
        let desktops = [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200];
        assert_eq!(timer_choices(3_600), desktops);
        assert_eq!(
            timer_choices(172_800),
            [0, 30, 300, 3_600, 28_800, 86_400, 172_800, 604_800, 2_419_200]
        );
        assert_eq!(timer_choice(0), "Off");
        assert_eq!(timer_choice(28_800), "8 hours");
        assert_eq!(timer_choice(2_419_200), "4 weeks");
    }

    #[test]
    fn the_timer_is_set_here_in_accepted_direct_chats_only() {
        assert!(can_set_timer(&chat("c:ana", "Ana")));
        assert!(
            !can_set_timer(&chat("g:01", "Family")),
            "a group's changes with the group"
        );
        assert!(!can_set_timer(&chat("c:PNI:p", "By number")));
        let blocked = Conversation {
            blocked: true,
            ..chat("c:b", "Blocked")
        };
        let request = Conversation {
            message_request: true,
            ..chat("c:s", "Stranger")
        };
        assert!(!can_set_timer(&blocked));
        assert!(!can_set_timer(&request));
    }
}

/// The options in a real (headless) window: `cargo test -p hush-desktop --features snapshot`.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AppContext as _, TestAppContext, WindowOptions};

    use super::super::attach::window_tests::with_core;
    use super::super::Hush;
    use crate::Demo;

    #[gpui::test]
    fn a_change_the_core_refuses_is_said(cx: &mut TestAppContext) {
        let (_, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Events, window, cx))
            })
            .expect("the window opens")
        });
        let data = std::env::temp_dir().join(format!("hush-desktop-options-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        // not linked: it sends nothing
        with_core(&hush, &data, cx);
        hush.update(cx, |app, cx| {
            let chat = app.selected.clone().unwrap();
            app.set_timer(chat, 30, cx);
        });
        cx.run_until_parked();
        let error = hush.read_with(cx, |app, _| app.error.clone().unwrap_or_default());
        assert!(error.starts_with("Couldn't set the disappearing timer: "), "{error}");
    }
}
