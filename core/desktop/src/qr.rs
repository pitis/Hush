use gpui_kit::{black, canvas, fill, point, px, size, white, Bounds, IntoElement, Styled};

/// The quiet border a scanner needs around the code, in modules.
const QUIET: usize = 4;

/// `text` as a QR code `side` pixels wide: whole pixels per module, so edges stay sharp.
pub fn code(text: &str, side: f32) -> Option<impl IntoElement> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width();
    let dark: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    let module = (side / (n + 2 * QUIET) as f32).floor().max(1.);
    Some(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                window.paint_quad(fill(bounds, white()));
                let inset = (side - module * n as f32) / 2.;
                for (i, _) in dark.iter().enumerate().filter(|(_, d)| **d) {
                    let (x, y) = ((i % n) as f32, (i / n) as f32);
                    let origin = point(
                        bounds.origin.x + px(inset + x * module),
                        bounds.origin.y + px(inset + y * module),
                    );
                    window.paint_quad(fill(Bounds::new(origin, size(px(module), px(module))), black()));
                }
            },
        )
        .size(px(side)),
    )
}
