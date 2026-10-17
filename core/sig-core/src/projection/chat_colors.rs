//! Signal's bubble colour presets (`ChatStyle.BubbleColorPreset` in backups), as the phone shows
//! them, and the chat styles of a history archive in our terms.

use super::model::ChatColor;

fn solid(c: u32) -> ChatColor {
    ChatColor {
        colors: vec![c],
        angle: 0,
    }
}

fn gradient(angle: u32, from: u32, to: u32) -> ChatColor {
    ChatColor {
        colors: vec![from, to],
        angle,
    }
}

/// The presets in `BubbleColorPreset` order (1 = Ultramarine … 22 = Tangerine).
pub fn presets() -> Vec<ChatColor> {
    vec![
        solid(0xFF2C6BED),                     // ultramarine
        solid(0xFFCF163E),                     // crimson
        solid(0xFFC73F0A),                     // vermilion
        solid(0xFF6F6A58),                     // burlap
        solid(0xFF3B7845),                     // forest
        solid(0xFF1D8663),                     // wintergreen
        solid(0xFF077D92),                     // teal
        solid(0xFF336BA3),                     // blue
        solid(0xFF6058CA),                     // indigo
        solid(0xFF9932C8),                     // violet
        solid(0xFFAA377A),                     // plum
        solid(0xFF8F616A),                     // taupe
        solid(0xFF71717F),                     // steel
        gradient(168, 0xFFE57C00, 0xFF5E0000), // ember
        gradient(180, 0xFF2C2C3A, 0xFF787891), // midnight
        gradient(192, 0xFFF65560, 0xFF442CED), // infrared
        gradient(180, 0xFF004066, 0xFF32867D), // lagoon
        gradient(192, 0xFFEC13DD, 0xFF1B36C6), // fluorescent
        gradient(180, 0xFF2F9373, 0xFF077343), // basil
        gradient(180, 0xFF6281D5, 0xFF974460), // sublime
        gradient(180, 0xFF498FD4, 0xFF2C66A0), // sea
        gradient(192, 0xFFDB7133, 0xFF911231), // tangerine
    ]
}

/// A preset by its `BubbleColorPreset` number; None for unknown (the default).
pub fn preset(number: i32) -> Option<ChatColor> {
    let i = usize::try_from(number).ok()?.checked_sub(1)?;
    presets().into_iter().nth(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_go_by_their_backup_numbers() {
        assert_eq!(presets().len(), 22);
        assert_eq!(preset(0), None, "unknown: the default");
        assert_eq!(preset(2), Some(solid(0xFFCF163E)), "crimson");
        assert_eq!(preset(22).map(|c| c.colors.len()), Some(2), "tangerine is a gradient");
        assert_eq!(preset(23), None);
        assert_eq!(preset(-1), None);
    }

    #[test]
    fn a_colour_is_stored_and_read_back() {
        for c in presets() {
            assert_eq!(ChatColor::from_row(&c.to_row()), Some(c));
        }
        assert_eq!(solid(0xFFCF163E).to_row(), "0:FFCF163E");
        for bad in ["", "0:", "x:FF000000", "0:zz", "FF000000"] {
            assert_eq!(ChatColor::from_row(bad), None, "{bad}");
        }
    }
}
