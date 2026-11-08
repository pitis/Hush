//! Hush for Windows, Linux and macOS: a GPUI window over the same core as the Mac app.

mod app;
mod key;
mod qr;
mod text;

use gpui_kit::*;

gpui_kit::actions!(hush, [Quit]);

#[cfg(target_os = "macos")]
const QUIT_KEY: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEY: &str = "ctrl-q";

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new(QUIT_KEY, Quit, None)]);
            cx.set_menus([Menu::new("Hush").items([MenuItem::action("Quit Hush", Quit)])]);
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1000.), px(720.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Hush".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| app::Hush::new(window, cx)))
                .expect("the window opens");
            cx.activate(true);
        });
}
