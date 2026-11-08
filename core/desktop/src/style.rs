//! The Mac app's look (`DESIGN.md`): macOS's own neutrals for the chrome, colour only in the
//! content. Sizes are the Mac app's points.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{rgb, rgba, App, Hsla};

#[derive(Clone, Copy)]
pub struct Palette {
    /// The conversation's background.
    pub window: Hsla,
    pub sidebar: Hsla,
    pub separator: Hsla,
    pub text: Hsla,
    pub secondary: Hsla,
    /// The system accent: selection, unread badges, the send button.
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub bubble_out: Hsla,
    pub bubble_in: Hsla,
    pub text_in: Hsla,
    /// The composer's field.
    pub field: Hsla,
    /// The sidebar's search field, the error banner.
    pub fill: Hsla,
    /// The chat list's selection while the window isn't active.
    pub selection: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
}

pub fn palette(cx: &App) -> Palette {
    if cx.theme().mode.is_dark() {
        Palette {
            window: rgb(0x1b1b1d).into(),
            sidebar: rgb(0x2b2b2d).into(),
            separator: rgba(0xffffff1a).into(),
            text: rgba(0xffffffd9).into(),
            secondary: rgb(0x9e9e9e).into(),
            accent: rgb(0x0a84ff).into(),
            on_accent: rgb(0xffffff).into(),
            bubble_out: rgb(0x2c6bed).into(),
            bubble_in: rgb(0x3b3b3d).into(),
            text_in: rgb(0xffffff).into(),
            field: rgb(0x2c2c2e).into(),
            fill: rgba(0xffffff1a).into(),
            selection: rgb(0x3a3a3c).into(),
            danger: rgb(0xff453a).into(),
            warning: rgb(0xffd60a).into(),
        }
    } else {
        Palette {
            window: rgb(0xffffff).into(),
            sidebar: rgb(0xf0f0f0).into(),
            separator: rgba(0x0000001a).into(),
            text: rgba(0x000000d9).into(),
            secondary: rgb(0x6b6b6b).into(),
            accent: rgb(0x007aff).into(),
            on_accent: rgb(0xffffff).into(),
            bubble_out: rgb(0x2c6bed).into(),
            bubble_in: rgb(0xe9e9e9).into(),
            text_in: rgb(0x1b1b1d).into(),
            field: rgb(0xf0f0f0).into(),
            fill: rgba(0x0000000f).into(),
            selection: rgb(0xdcdcdc).into(),
            danger: rgb(0xff3b30).into(),
            warning: rgb(0xffcc00).into(),
        }
    }
}

/// The avatar colours (the Mac app's `Theme.avatarPalette`).
const AVATARS: [u32; 8] = [
    0xdb5c5c, 0xe38c45, 0xcca833, 0x5cad6b, 0x409ead, 0x527dd9, 0x8f6bd6, 0xcc66a8,
];

/// A chat's or a person's colour, the same on every launch: FNV-1a over the id (`c:<aci>` for a
/// person), as `Formatting.avatarIndex`.
pub fn avatar_color(id: &str) -> Hsla {
    let hash = id
        .bytes()
        .fold(2_166_136_261u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16_777_619));
    rgb(AVATARS[hash as usize % AVATARS.len()]).into()
}

/// Up to two initials: each space-separated word's first letter or digit, uppercased; "#" when
/// there are none (`Formatting.initials`).
pub fn initials(name: &str) -> String {
    let firsts = name
        .split(' ')
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()));
    let out: String = firsts.take(2).flat_map(char::to_uppercase).collect();
    if out.is_empty() {
        "#".into()
    } else {
        out
    }
}

/// A bubble's fill from a chat colour (0xAARRGGBB colours at `angle` degrees, 0 bottom to top):
/// one colour is solid, more are a gradient of the first and last.
pub fn bubble_fill(color: Option<&sig_core::projection::ChatColor>, fallback: Hsla) -> gpui_kit::Background {
    let argb = |c: u32| -> Hsla { rgba(c.rotate_left(8)).into() };
    match color.map(|c| (c.colors.as_slice(), c.angle)) {
        Some(([one], _)) => argb(*one).into(),
        Some(([first, .., last], angle)) => gpui_kit::linear_gradient(
            angle as f32,
            gpui_kit::linear_color_stop(argb(*first), 0.),
            gpui_kit::linear_color_stop(argb(*last), 1.),
        ),
        _ => fallback.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_take_the_first_letter_or_digit_of_two_words() {
        assert_eq!(initials("Note to Self"), "NT");
        assert_eq!(initials("@bob"), "B");
        assert_eq!(initials("ana"), "A");
        assert_eq!(initials("🎉 party 2024"), "P2");
        assert_eq!(initials("🎉"), "#");
        assert_eq!(initials(""), "#");
    }

    #[test]
    fn avatar_colours_follow_fnv_1a() {
        // FNV-1a of "a" is 0xe40c292c: index 4 of 8
        assert_eq!(avatar_color("a"), Hsla::from(rgb(AVATARS[0xe40c292c % 8])));
        assert_eq!(avatar_color("c:x"), avatar_color("c:x"));
    }
}
