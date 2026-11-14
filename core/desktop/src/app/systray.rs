//! The tray icon, in the menu bar on macOS (the Mac app's `MenuBarItem`): the unread count, and a
//! menu to open the window, jump to a chat with unread messages, or quit; Settings takes it down.
//! On macOS a click opens the menu; elsewhere a left click opens the window and a right click the
//! menu. The count is the Dock's (`notifications`); the window still quits Hush when closed.

use futures::channel::mpsc::unbounded;
use futures::StreamExt;
use gpui_kit::assets::AllAssets;
use gpui_kit::*;
use sig_core::projection::Conversation;
use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::notifications::{unread, APP_ID};
use super::settings::{chosen, Choice};
use super::Hush;
use crate::text;

/// How many unread chats the menu lists (`MenuBar.unreadChatsShown`).
const CHATS_SHOWN: usize = 5;
/// The menu's items: these, and a chat's, `CHAT` then its id.
const OPEN: &str = "open";
const QUIT: &str = "quit";
const CHAT: &str = "chat:";

/// What the tray shows: drawn again only when this changes.
#[derive(Debug, PartialEq)]
struct Shown {
    unread: u32,
    /// The unread chats listed: id, and its entry.
    chats: Vec<(String, String)>,
}

#[derive(Default)]
pub(super) struct Systray {
    /// None while Settings hides it.
    icon: Option<TrayIcon>,
    /// None until first drawn.
    shown: Option<Shown>,
}

/// Settings' Show in menu bar (Show in system tray off macOS).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum ShowInTray {
    #[default]
    Shown,
    Hidden,
}

impl Global for ShowInTray {}

impl Choice for ShowInTray {
    const FILE: &'static str = "tray";
    const ALL: &'static [(Self, &'static str, &'static str)] =
        &[(Self::Shown, "shown", "Shown"), (Self::Hidden, "hidden", "Hidden")];
}

/// The chats with unread messages, in list order (pinned, then newest), as the menu lists them:
/// the ones the unread count counts (`ChatList.unreadChats`), each with its count
/// (`ChatList.menuTitle`).
fn unread_chats(chats: &[Conversation], now_ms: i64, own_aci: Option<&str>) -> Vec<(String, String)> {
    let entry = |c: &Conversation| {
        // `&` marks the next letter as the item's key
        let title = text::title(c, own_aci).replace('&', "&&");
        match c.unread_count {
            0 => title,
            n => format!("{title} ({n})"),
        }
    };
    chats
        .iter()
        .filter(|c| unread(c, now_ms) > 0)
        .take(CHATS_SHOWN)
        .map(|c| (c.id.clone(), entry(c)))
        .collect()
}

/// Puts the tray up, its clicks going to the window's app (once the app runs: macOS wants its
/// event loop going).
pub fn start(window: AnyWindowHandle, hush: WeakEntity<Hush>, cx: &mut App) {
    let (picks, mut picked) = unbounded::<String>();
    let clicks = picks.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = picks.unbounded_send(event.id.0);
    }));
    // on every system: without a handler, tray-icon keeps each event (on macOS each hover) for good
    TrayIconEvent::set_event_handler(Some(move |event| {
        let left_click = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        );
        // macOS opens the menu on any click
        if left_click && !cfg!(target_os = "macos") {
            let _ = clicks.unbounded_send(OPEN.into());
        }
    }));
    let _ = hush.update(cx, |app, cx| {
        app.systray_shown(cx);
        cx.observe_global::<ShowInTray>(Hush::systray_shown).detach();
    });
    cx.spawn(async move |cx| {
        while let Some(id) = picked.next().await {
            let done = window.update(cx, |_, window, cx| {
                hush.update(cx, |app, cx| app.tray_picked(&id, window, cx))
            });
            if let Err(e) = done.and_then(|r| r) {
                tracing::warn!(error = %e, "a tray click went nowhere");
            }
        }
    })
    .detach();
}

impl Hush {
    /// The chats or the unread count changed (`MenuBarItem.update`, and its menu as it opens).
    pub(super) fn systray_changed(&mut self, now_ms: i64) {
        let Some(icon) = &self.systray.icon else { return };
        let shown = Shown {
            unread: self.notifications.badge,
            chats: unread_chats(&self.chats, now_ms, self.own_aci.as_deref()),
        };
        let before = self.systray.shown.as_ref();
        if before == Some(&shown) {
            return;
        }
        let changed = before.is_none_or(|b| (b.unread > 0) != (shown.unread > 0));
        if let Some(picture) = changed.then(|| picture(shown.unread > 0)).flatten() {
            // drawn in the menu bar's colour
            #[cfg(target_os = "macos")]
            let set = icon.set_icon_templated(Some(picture));
            #[cfg(not(target_os = "macos"))]
            let set = icon.set_icon(Some(picture));
            if let Err(e) = set {
                tracing::warn!(error = %e, "the tray's picture didn't change");
            }
        }
        let unread = (shown.unread > 0).then(|| shown.unread.to_string());
        // the count beside the picture; Windows has no title, and Linux's names the app
        if cfg!(target_os = "macos") {
            icon.set_title(unread.as_ref().map(|n| format!(" {n}")));
        }
        let tooltip = unread.map_or_else(|| "Hush".into(), |n| format!("Hush, {n} unread"));
        if let Err(e) = icon.set_tooltip(Some(tooltip)) {
            tracing::warn!(error = %e, "the tray's tooltip didn't change");
        }
        match menu(&shown) {
            Ok(menu) => {
                icon.set_menu(Some(Box::new(menu)));
                self.systray.shown = Some(shown);
            }
            // tried again at the next change
            Err(e) => tracing::warn!(error = %e, "the tray's menu didn't change"),
        }
    }

    /// Puts the icon up or takes it down, as Settings has it: taken down, as hiding it would leave
    /// it in KDE's tray, among the hidden ones.
    fn systray_shown(&mut self, cx: &mut Context<Self>) {
        let shown = chosen::<ShowInTray>(cx) == ShowInTray::Shown;
        if shown == self.systray.icon.is_some() {
            return;
        }
        self.systray = Systray {
            icon: shown.then(build).flatten(),
            shown: None,
        };
        self.systray_changed(chrono::Utc::now().timestamp_millis());
    }

    /// An item of the tray's menu, or a click on the tray (`OPEN`).
    fn tray_picked(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match id {
            QUIT => super::quit(cx),
            _ => {
                cx.activate(true);
                window.activate_window();
                let chat = id
                    .strip_prefix(CHAT)
                    .filter(|id| self.chats.iter().any(|c| c.id == *id));
                if let Some(chat) = chat {
                    self.select(chat.to_owned(), window, cx);
                }
            }
        }
    }
}

/// The icon, its picture and menu to come (macOS wants its event loop going by now).
fn build() -> Option<TrayIcon> {
    let builder = TrayIconBuilder::new()
        // the same each launch: KDE keeps what's chosen for the item by it
        .with_id(APP_ID)
        .with_tooltip("Hush")
        .with_menu_on_left_click(cfg!(target_os = "macos"));
    // the app's name on Linux; macOS shows the count there, and Windows nothing
    let builder = if cfg!(target_os = "macos") {
        builder
    } else {
        builder.with_title("Hush")
    };
    match builder.build() {
        Ok(icon) => {
            #[cfg(not(any(target_os = "macos", windows)))]
            warn_without_host();
            Some(icon)
        }
        Err(e) => {
            tracing::warn!(error = %e, "no tray icon");
            None
        }
    }
}

/// Linux: the icon is put up whether or not a tray shows such icons (GNOME's doesn't without the
/// AppIndicator extension): said in the log when none does.
#[cfg(not(any(target_os = "macos", windows)))]
fn warn_without_host() {
    let ask = || -> zbus::Result<bool> {
        let bus = zbus::blocking::Connection::session()?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&bus)?;
        Ok(dbus.name_has_owner("org.kde.StatusNotifierWatcher".try_into()?)?)
    };
    // off the main thread: the bus can be slow to answer
    let asked = std::thread::Builder::new()
        .name("tray-host".into())
        .spawn(move || match ask() {
            Ok(true) => {}
            Ok(false) => tracing::warn!("no tray shows the icon: no StatusNotifierItem host is running"),
            Err(e) => tracing::warn!(error = %e, "the tray's host couldn't be asked for"),
        });
    if let Err(e) = asked {
        tracing::warn!(error = %e, "the tray's host couldn't be asked for");
    }
}

fn menu(shown: &Shown) -> tray_icon::menu::Result<Menu> {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(OPEN, "Open Hush", true, None))?;
    if !shown.chats.is_empty() {
        menu.append(&PredefinedMenuItem::separator())?;
        for (id, entry) in &shown.chats {
            menu.append(&MenuItem::with_id(format!("{CHAT}{id}"), entry, true, None))?;
        }
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let quit_key = cfg!(target_os = "macos").then(|| Accelerator::new(Modifiers::META, Code::KeyQ));
    menu.append(&MenuItem::with_id(QUIT, "Quit Hush", true, quit_key))?;
    Ok(menu)
}

/// The picture's side in pixels: the menu bar's 22 points, on a Retina screen.
const SIDE: u32 = 44;

fn picture(unread: bool) -> Option<Icon> {
    let drawn = drawn(unread).ok_or_else(|| "the icon didn't draw".to_string());
    let icon = drawn.and_then(|rgba| Icon::from_rgba(rgba, SIDE, SIDE).map_err(|e| e.to_string()));
    icon.inspect_err(|e| tracing::warn!(error = %e, "no tray picture")).ok()
}

/// Lucide's two bubbles (the Mac app's SF Symbol), one filled while something is unread: black
/// for the menu bar to tint, elsewhere the accent, seen on light and dark taskbars. RGBA.
fn drawn(unread: bool) -> Option<Vec<u8>> {
    use resvg::tiny_skia::{Pixmap, Transform};
    let svg = AllAssets.load("icons/messages-square.svg").ok()??;
    let color = if cfg!(target_os = "macos") { "black" } else { "#007aff" };
    let mut svg = String::from_utf8_lossy(&svg).replace("currentColor", color);
    if unread {
        // the left bubble: the right one's path isn't closed
        svg = svg.replacen("<path ", &format!("<path fill=\"{color}\" "), 1);
    }
    let tree = resvg::usvg::Tree::from_str(&svg, &Default::default()).ok()?;
    let mut pixmap = Pixmap::new(SIDE, SIDE)?;
    let scale = SIDE as f32 / tree.size().width();
    resvg::render(&tree, Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let straight = pixmap.pixels().iter().map(|p| p.demultiply());
    Some(
        straight
            .flat_map(|p| [p.red(), p.green(), p.blue(), p.alpha()])
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{unread_chats, Choice, ShowInTray};
    use sig_core::projection::Conversation;

    const NOW: i64 = 1_000_000;

    fn chat(id: &str, title: &str, unread: u32) -> Conversation {
        Conversation {
            unread_count: unread,
            ..crate::app::tests::chat(id, title)
        }
    }

    #[test]
    fn the_menu_lists_the_unread_chats_the_count_counts() {
        let chats = [
            chat("c:a", "Ana", 2),
            chat("c:read", "Read", 0),
            Conversation {
                archived: true,
                ..chat("c:archived", "Archived", 4)
            },
            Conversation {
                mute_until: NOW + 1,
                ..chat("c:muted", "Muted", 1)
            },
            Conversation {
                mute_until: NOW,
                ..chat("c:mute-over", "Mute over", 1)
            },
            Conversation {
                marked_unread: true,
                ..chat("g:1", "Tom & Jerry", 0)
            },
            chat("c:me", "", 1),
        ];
        let listed = unread_chats(&chats, NOW, Some("me"));
        let listed: Vec<(&str, &str)> = listed.iter().map(|(id, entry)| (id.as_str(), entry.as_str())).collect();
        assert_eq!(
            listed,
            [
                ("c:a", "Ana (2)"),
                ("c:mute-over", "Mute over (1)"),
                // marked unread: no count; `&` alone would underline the next letter
                ("g:1", "Tom && Jerry"),
                ("c:me", "Note to Self (1)"),
            ]
        );
    }

    #[test]
    fn the_menu_lists_five_chats_at_most_newest_first() {
        let chats: Vec<Conversation> = (1..=7).map(|n| chat(&format!("c:{n}"), &format!("C{n}"), n)).collect();
        let listed: Vec<String> = unread_chats(&chats, NOW, None).into_iter().map(|(id, _)| id).collect();
        assert_eq!(listed, ["c:1", "c:2", "c:3", "c:4", "c:5"]);
    }

    #[test]
    fn the_tray_shows_unless_hidden_in_settings() {
        let folder = std::env::temp_dir().join(format!("hush-desktop-tray-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(ShowInTray::read(&folder), ShowInTray::Shown, "none kept yet");
        ShowInTray::Hidden.save(&folder).unwrap();
        assert_eq!(ShowInTray::read(&folder), ShowInTray::Hidden);
        assert_eq!(std::fs::read_to_string(folder.join("tray")).unwrap(), "hidden");
        ShowInTray::Shown.save(&folder).unwrap();
        assert_eq!(ShowInTray::read(&folder), ShowInTray::Shown);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_picture_is_drawn_filled_while_something_is_unread() {
        let opaque = |unread| {
            let drawn = super::drawn(unread).expect("drawn");
            assert_eq!(drawn.len(), (super::SIDE * super::SIDE * 4) as usize);
            drawn.chunks(4).filter(|p| p[3] > 0).count()
        };
        assert!(opaque(false) > 0);
        assert!(opaque(true) > opaque(false));
    }
}

/// Picks from the tray's menu in a real (headless) window over the made-up chats.
#[cfg(all(test, feature = "snapshot"))]
mod window_tests {
    use gpui_kit::{gpui, AppContext as _, TestAppContext, VisualTestContext, WindowOptions};

    use super::super::Hush;
    use super::{CHAT, OPEN};
    use crate::demo::ANA;
    use crate::Demo;

    #[gpui::test]
    fn a_pick_brings_up_the_window_and_its_chat(cx: &mut TestAppContext) {
        let (window, hush) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Hush::demo(Demo::Chat, window, cx))
            })
            .expect("the window opens")
        });
        cx.run_until_parked();
        let pick = |id: String, cx: &mut TestAppContext| {
            VisualTestContext::from_window(window, cx).deactivate_window();
            window
                .update(cx, |_, window, cx| {
                    hush.update(cx, |app, cx| app.tray_picked(&id, window, cx))
                })
                .expect("the window is open");
            cx.run_until_parked();
            hush.read_with(cx, |app, _| (app.window_active, app.selected.clone()))
        };
        let ana = format!("c:{ANA}");
        assert_eq!(pick(format!("{CHAT}{ana}"), cx), (true, Some(ana.clone())));
        // gone from the list since the menu was drawn (unlinked, deleted): the window only
        assert_eq!(pick(format!("{CHAT}c:gone"), cx), (true, Some(ana.clone())));
        assert_eq!(pick(OPEN.into(), cx), (true, Some(ana)));
    }
}
