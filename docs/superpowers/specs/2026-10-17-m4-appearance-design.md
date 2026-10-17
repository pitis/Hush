# Light and dark appearance (M4 design)

## Context
Issue #55: "Theme: system, light, dark." The app was dark only (`.preferredColorScheme(.dark)`,
a fixed palette after Signal Desktop's dark theme, white words in every bubble).

## Goals
1. Settings ▸ Appearance: System, Light, Dark. Dark stays the default (the app's look so far).
2. In Light, the app follows Signal Desktop's light theme: white window, light grey sidebar and
   incoming bubbles with dark words; outgoing bubbles stay blue with white words.

## Design
- `Appearance` (stored in user defaults) sets the root view's colour scheme (`nil` for System).
- `Theme` colours are dynamic `NSColor`s with a light and a dark value, picked by the drawing
  appearance; `Theme.incomingText` is the incoming bubble's words.
- In bubbles, dimmed words (deleted, time, poll hints) and poll bars use hierarchical styles of
  the bubble's own foreground; a spoiler's cover is the words' colour.

## Not in this slice
Chat colours and wallpapers (synced from the phone's account record).

## Testing
- The choices map to colour schemes, Dark first by default; theme colours resolve light in Aqua
  and dark in Dark Aqua, incoming words the other way. Both themes checked from snapshots.
