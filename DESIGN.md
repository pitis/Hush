---
name: Hush
description: A native Mac client for Signal. Quiet, precise, native.
colors:
  bubble-outgoing: "#2C6BED"
  bubble-incoming-light: "#E9E9E9"
  bubble-incoming-dark: "#3B3B3D"
  secondary-text-light: "#6B6B6B"
  secondary-text-dark: "#9E9E9E"
typography:
  chat-title:
    fontFamily: "SF Pro (system)"
    fontSize: "13px"
    fontWeight: 600
  message:
    fontFamily: "SF Pro (system)"
    fontSize: "14px"
    fontWeight: 400
  preview:
    fontFamily: "SF Pro (system)"
    fontSize: "13px"
    fontWeight: 400
  meta:
    fontFamily: "SF Pro (system)"
    fontSize: "12px"
    fontWeight: 400
  bubble-time:
    fontFamily: "SF Pro (system)"
    fontSize: "10px"
    fontWeight: 400
rounded:
  field: "18px"
  panel: "16px"
  bubble: "18px"
components:
  composer-field:
    rounded: "{rounded.field}"
    padding: "9px 14px"
  composer-panel:
    rounded: "{rounded.panel}"
    padding: "8px"
---

# Design System: Hush

## 1. Overview

**Creative North Star: "The Quiet Window"**

Hush is a Mac window first and a messenger second. macOS 26 draws the structure (the window, the
glass sidebar, the toolbar, selection, search) and Hush stays out of its way; the conversation is
the loudest thing on screen. The system rejects what `PRODUCT.md` names: a web app in a window,
glass as decoration, busy colour, a generic chat template.

**Key Characteristics:**
- The system's chrome, never painted over: no app-coloured window, sidebar or toolbar fills.
- Colour lives in the content: bubbles and chat colours. Selection and badges use the system
  accent.
- Glass only on the layer that floats over content: the toolbar and sidebar (the system's), the
  composer, the "New messages" pill.
- Follows the system appearance; light and dark are both first-class.

## 2. Colors

Restrained: system neutrals and materials everywhere, with colour reserved for messages.

### Primary
- **Outgoing Blue** (#2C6BED): outgoing bubbles (each chat's colour may replace it). Never chrome.

### Neutral
- **Incoming Grey** (#E9E9E9 light / #3B3B3D dark): incoming bubbles.
- **Secondary Text** (#6B6B6B light / #9E9E9E dark): timestamps, notices, meta lines.
- The main window's chrome is the system's: window and sidebar backgrounds, the chat list's
  selection, separators, the accent (unread badges, the picked mention).
- In-content greys stay Hush's for now (`Theme.field`, `Theme.selection`): the reaction pill,
  file chips, the search result picked with the arrows, the sticker picker. `Theme.window` is
  left only in the call window and the history overlay.

**The Content Carries Colour Rule.** If a fill sits behind chrome (a sidebar, a header, a
banner), it is the system's, or it is wrong.

## 3. Typography

**Font:** SF Pro, the system font, at fixed sizes (no display faces).

### Hierarchy
- **Chat title** (semibold 13, bold when unread): chat list rows.
- **Message** (regular 14): message text in bubbles and in the composer.
- **Preview** (regular 13): the chat list's second line.
- **Meta** (regular 12): list times, notices, accessory details; semibold 11 for accessory
  titles; 11 and 12 in search results and the link preview card.
- **Bubble time** (regular 10): the time and ticks inside a bubble.
- **Window title and subtitle:** the system's (`navigationTitle` / `navigationSubtitle`).

## 4. Elevation

No shadows of Hush's own. Depth comes from the system's layers: content at the bottom, the
glass control layer floating above it (toolbar, sidebar, composer), sheets and popovers above
that. Messages scroll under the composer and the toolbar, and the scroll-edge effect separates
them; there are no divider lines.

**The Glass Is Structure Rule.** Glass marks a layer that floats over content. It never goes on
bubbles, notices, rows, search results or cards inside the content.

## 5. Components

### Composer
- One `GlassEffectContainer`: a round glass "+" menu (Attach Files… ⇧⌘A, Stickers…) and the
  glass message field (continuous 18pt corners, 9×14 padding). The "+" is as tall as a one-line
  field, so it sits centred beside it and stays by the last line as the field grows.
- A send button (accent `arrow.up.circle.fill`, 24pt) appears at the field's trailing edge while
  there is something to send, centred on the last line; ↩ does the same.
- Accessories (mention picker, link preview, reply and edit bars, file tray) share one glass
  panel above the field (continuous 16pt corners, 8pt padding). Reply and edit bars lead with an
  icon, never a coloured stripe.

### Chat list
- A native sidebar `List` with system selection; rows are a 40pt avatar, title, one-line
  preview, time, and an accent unread capsule.
- Native sidebar search; the chat scope ("In …") is a row with "All Chats".
- New Chat and Stories are toolbar items; connection status is one quiet line at the foot.

### Conversation toolbar
- The chat's name is the window title; the subtitle says the most pressing thing in words
  (typing, offline, the disappearing timer, @username).
- Trailing: Voice Call and one Chat Options menu (timer, colour and wallpaper, search in chat,
  group info, the note).

### Typing bubble
- While someone types, the timeline ends with an incoming bubble of three dots pulsing left to
  right (still under Reduce Motion), after the typer's picture in a group. The toolbar subtitle
  also says "typing…", for when the bottom is scrolled out of view.

### Notices
- Centred, secondary 12pt with an SF Symbol. Repeated decryption failures collapse into one
  notice with one link-style action that asks before it acts.

### Error banner
- At the top of the chat list in a rounded quaternary shape with a tinted icon; never a
  full-bleed fill.

## 6. Do's and Don'ts

### Do:
- **Do** let the system draw the window, sidebar, toolbar, search and selection.
- **Do** name people by name wherever the name is known; never show an enum case.
- **Do** collapse repeated notices into one, with one action.
- **Do** keep every keyboard path, including ⌘K, ⌘F, ⇧⌘F, ⌥↑↓, ⌥⇧↑↓, ⌘1–9, ⇧⌘T, ⇧⌘C, ↩, ⇧↩, ↑,
  Esc and ⇧⌘A.

### Don't:
- **Don't** look like a web app in a window: no opaque panel fills, no custom title bars, no
  hand-built search pills or sidebar titles.
- **Don't** use glass as decoration: never on content.
- **Don't** go busy or colourful: no accent on inactive states, no competing badges.
- **Don't** look like a generic chat template or an AI-made mockup.
- **Don't** use a coloured side stripe to mark a reply, an edit or a notice.
