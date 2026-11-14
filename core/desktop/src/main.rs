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
            cx.on_action(|_: &Quit, cx| app::quit(cx));
            cx.bind_keys([KeyBinding::new(QUIT_KEY, Quit, None)]);
            cx.set_menus([Menu::new("Hush").items([MenuItem::action("Quit Hush", Quit)])]);
            cx.on_window_closed(|cx, _| app::quit(cx)).detach();
            gpui_kit::open_window(window_options(cx), cx, |window, cx| {
                cx.new(|cx| app::Hush::new(window, cx))
            })
            .expect("the window opens");
            cx.activate(true);
        });
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
    /// A chat's pictures, recordings and files.
    Files,
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
                ("files", Demo::Files),
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
                        for _ in 0..4 {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(150))
                                .await;
                            let _ = window.update(cx, |_, window, cx| {
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
