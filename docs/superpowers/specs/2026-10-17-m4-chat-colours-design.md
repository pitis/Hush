# Chat colours and wallpapers (M4 design)

## Context
Issue #55: chat colours and wallpapers, ideally from the phone. Storage Service records carry
neither. The history archive at link time does: each `Chat.style`, plus the account's default
style and custom colours.

## Goals
1. Our bubbles take the chat's colour: its own, else the phone's default, else the app's blue.
2. At link time with history, each chat's colour and the default come from the phone (a preset or
   a custom solid/gradient colour).
3. A chat's colour can be changed here (header ▸ palette): Signal's 22 presets, or the default.
   The change stays on this Mac (no sync channel carries it).

## Design
- `ChatColor { colors (0xAARRGGBB), angle }`, stored as `<angle>:<hex,…>` in
  `conversations.chat_color`. The default is in `app_values.default_chat_color` (migration 0021).
  `conversations()` reads the chat's own colour, else the default.
- `projection::chat_colors::presets()` is Signal's palette, numbered by `BubbleColorPreset`. The
  values are Signal's palette as the phone shows it. The app gets them from `chat_color_presets()`.
- Import: the account frame's custom colours are kept by id, and `defaultChatStyle` becomes the
  batch's default. A chat's style resolves a preset or custom id. Automatic means none (with no
  wallpaper it is the default blue). Re-importing sets what the phone says.
- App: an environment value carries the chat's colour to `MessageRow`. Gradients use Signal's angle
  (0° bottom to top, 90° left to right).

## Wallpapers
- Each chat's wallpaper preset and the phone's default come over the same way: `Chat.style` and
  `defaultChatStyle`, stored in `conversations.wallpaper` and `app_values.default_wallpaper`
  (migration 0022). Photo wallpapers aren't brought over.
- `wallpaper_presets()` gives Signal's 21 presets, numbered by `WallpaperPreset`. The popover in
  the header picks a colour and a wallpaper, or puts either back to the default.
- The wallpaper fills the timeline and is dimmed in dark mode. Date separators and chat events
  (missed calls, timer changes) get a material pill over a wallpaper, so they stay readable.

## Not in this slice
Photo wallpapers; automatic bubble colours that follow a wallpaper.

## Testing
- Core: preset numbering; row round trip; import of custom gradient, preset and automatic, and the
  default; a chat shows its own colour, else the default, and can be set back to it.
- App: gradient points for 0°, 90°, 180°; ARGB reading; presets; choosing asks the core. Picker
  and a gradient bubble checked from snapshots.
