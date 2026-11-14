//! Hush for Windows, Linux and macOS: a GPUI window over the same core as the Mac app.

mod app;
#[cfg(feature = "snapshot")]
mod demo;
mod key;
mod qr;
mod style;
mod text;

use gpui_kit::*;

gpui_kit::actions!(hush, [Quit]);

#[cfg(target_os = "macos")]
const QUIT_KEY: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEY: &str = "ctrl-q";

fn main() {
    #[cfg(feature = "snapshot")]
    if let [_, flag, dir] = std::env::args().collect::<Vec<_>>().as_slice() {
        if flag == "--snapshot" {
            return snapshot(dir.into());
        }
    }
    // the whole Lucide set: the default one lacks icons used here (the clock, for one)
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            // before any window or notification
            cx.set_app_identity(app::notifications::APP_ID, "Hush");
            cx.on_action(|_: &Quit, cx| app::quit(cx));
            cx.bind_keys(key_bindings());
            cx.set_menus([
                Menu::new("Hush").items([
                    MenuItem::action("Settings…", app::settings::OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Quit Hush", Quit),
                ]),
                Menu::new("Chats").items(chats_menu()),
            ]);
            cx.on_window_closed(|cx, _| app::quit(cx)).detach();
            let (window, hush) = gpui_kit::open_window(window_options(cx), cx, |window, cx| {
                cx.new(|cx| app::Hush::new(window, cx))
            })
            .expect("the window opens");
            app::search::shortcuts(window, hush.downgrade(), cx);
            app::attach::shortcut(window, hush.downgrade(), cx);
            app::navigate::shortcuts(window, hush.downgrade(), cx);
            app::settings::shortcut(window, hush.downgrade(), cx);
            app::notifications::responses(window, hush.downgrade(), cx);
            cx.activate(true);
        });
}

/// Every shortcut the app binds, before the menus are set (which show them).
fn key_bindings() -> Vec<KeyBinding> {
    let mut all = vec![KeyBinding::new(QUIT_KEY, Quit, None)];
    all.extend(app::search::keys());
    all.extend(app::compose::keys());
    all.extend(app::attach::keys());
    all.extend(app::navigate::keys());
    all.extend(app::settings::keys());
    all
}

/// The Mac app's Chats menu (`ChatCommands`).
fn chats_menu() -> Vec<MenuItem> {
    use app::navigate::*;
    let jump = (1..=9).map(|n| MenuItem::action(format!("Chat {n}"), JumpToChat(n)));
    vec![
        MenuItem::action("Find Chat…", FindChat),
        MenuItem::action("Search", app::search::Search),
        MenuItem::action("Search in Chat", app::search::SearchInChat),
        MenuItem::separator(),
        MenuItem::action("Previous Chat", PreviousChat),
        MenuItem::action("Next Chat", NextChat),
        MenuItem::action("Previous Unread Chat", PreviousUnreadChat),
        MenuItem::action("Next Unread Chat", NextUnreadChat),
        MenuItem::submenu(Menu::new("Jump to Chat").items(jump)),
        MenuItem::separator(),
        MenuItem::action("Focus Composer", FocusComposer),
        MenuItem::action("Attach Files…", app::attach::AttachFiles),
        MenuItem::action("Close Chat", CloseChat),
    ]
}

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1000.), px(720.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Hush".into()),
            // macOS: the window buttons sit in the sidebar, as in the Mac app's split view
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: Some(point(px(22.), px(22.))),
        }),
        window_min_size: Some(size(px(820.), px(520.))),
        // Linux: ties the window to the launcher entry named `APP_ID` (the one with the unread count)
        app_id: Some(app::notifications::APP_ID.into()),
        ..Default::default()
    }
}

/// The screens `--snapshot` renders.
#[cfg(feature = "snapshot")]
#[derive(Clone, Copy)]
pub enum Demo {
    Link,
    Empty,
    Chat,
    /// The chat, with the New Group dialog over it.
    NewGroup,
    /// The chat, replying to a message whose buttons and reaction picker show.
    Reply,
    /// A chat's pictures, recordings and files, and files waiting to be sent.
    Files,
    /// The chat, writing a styled message with a mention, the mention picker open.
    Compose,
    /// Every chat searched.
    Search,
    /// The open chat searched, its newest match opened.
    SearchInChat,
    /// The chat scrolled back: an older page loading, a message come in unseen.
    History,
    /// The chat with a received link preview and a group's invite link, writing a link with its
    /// preview above the composer.
    LinkPreview,
    /// A chat with disappearing messages on, and events of each kind among its messages.
    Events,
    /// That chat in its colour, on its wallpaper, the picker open.
    ChatStyle,
    /// The chat, a row's right-click menu open.
    ChatMenu,
    /// A message request open: its answers in place of the composer.
    Request,
    /// The archived chats listed.
    Archive,
    /// The Family group's info, as an admin sees it.
    GroupInfo,
    /// Its name and description being edited.
    EditGroup,
    /// Members being added to it.
    AddMembers,
    /// Settings over the chat, with our made-up profile.
    Settings,
    /// Removed on the phone: Link Again.
    Unlinked,
}

/// `hush --snapshot <folder>`: renders made-up screens, light and dark, to PNGs there and quits,
/// to check the UI where the screen can't be captured:
/// `cargo run -p hush-desktop --features snapshot -- --snapshot <folder>`.
#[cfg(feature = "snapshot")]
fn snapshot(dir: std::path::PathBuf) {
    use gpui_kit::component::{Theme, ThemeMode};
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            let shots = [
                ("link", Demo::Link),
                ("empty", Demo::Empty),
                ("chat", Demo::Chat),
                ("new-group", Demo::NewGroup),
                ("reply", Demo::Reply),
                ("files", Demo::Files),
                ("compose", Demo::Compose),
                ("search", Demo::Search),
                ("search-in-chat", Demo::SearchInChat),
                ("history", Demo::History),
                ("link-preview", Demo::LinkPreview),
                ("events", Demo::Events),
                ("chat-style", Demo::ChatStyle),
                ("chat-menu", Demo::ChatMenu),
                ("request", Demo::Request),
                ("archive", Demo::Archive),
                ("group-info", Demo::GroupInfo),
                ("edit-group", Demo::EditGroup),
                ("add-members", Demo::AddMembers),
                ("settings", Demo::Settings),
                ("unlinked", Demo::Unlinked),
            ];
            cx.spawn(async move |cx| {
                for (mode, theme) in [("light", ThemeMode::Light), ("dark", ThemeMode::Dark)] {
                    for (name, screen) in shots {
                        cx.update(|cx| Theme::change(theme, None, cx));
                        let opened = cx.update(|cx| {
                            let options = WindowOptions {
                                show: false,
                                focus: false,
                                ..window_options(cx)
                            };
                            gpui_kit::open_window(options, cx, |window, cx| {
                                cx.new(|cx| app::Hush::demo(screen, window, cx))
                            })
                        });
                        let Ok((window, _)) = opened else { continue };
                        // layout settles over a few frames (the timeline measures its rows)
                        for frame in 0..4 {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(150))
                                .await;
                            let _ = window.update(cx, |_, window, cx| {
                                if frame == 1 && matches!(screen, Demo::ChatMenu) {
                                    right_click_ana(window, cx);
                                }
                                window.refresh();
                                let _ = window.draw(cx);
                            });
                        }
                        let path = dir.join(format!("{name}-{mode}.png"));
                        match window.update(cx, |_, window, _| window.render_to_image()) {
                            Ok(Ok(image)) => eprintln!("{}: {:?}", path.display(), image.save(&path)),
                            Ok(Err(e)) => eprintln!("{}: {e}", path.display()),
                            Err(e) => eprintln!("{}: {e}", path.display()),
                        }
                        let _ = window.update(cx, |_, window, _| window.remove_window());
                    }
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
}

/// A right-click on Ana's row, under the search field, the pinned Family and the Chats title, and
/// on macOS the window buttons' strip (the rows laid out by a frame already).
#[cfg(feature = "snapshot")]
fn right_click_ana(window: &mut Window, cx: &mut App) {
    let at = point(px(120.), px(if cfg!(target_os = "macos") { 238. } else { 194. }));
    let modifiers = Modifiers::default();
    let events = [
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: None,
            modifiers,
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Right,
            position: at,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Right,
            position: at,
            modifiers,
            click_count: 1,
        }),
    ];
    for event in events {
        window.dispatch_event(event, cx);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_shortcut_parses() {
        // `KeyBinding::new` panics on a keystroke it can't read (`secondary-`, a typo): at launch
        assert!(!super::key_bindings().is_empty());
    }
}
