# Native look on macOS 26 (#96)

## Summary

Hush stops painting over macOS 26. The window, sidebar, conversation header and composer use the
system's own structure and Liquid Glass; Hush's own colour stays in the message bubbles and chat
colours. Repeated decryption notices collapse into one row in plain words. Hush requires macOS 26
and follows the system appearance. Guided by `PRODUCT.md` (quiet, precise, native).

The critique that led here (21/40): opaque Signal Desktop greys over the system's glass sidebar and
toolbar, a doubled header (~106pt of chrome before the first message), hand-built sidebar
furniture, a composer that clips the timeline, and core-log text and raw ids shown as UI copy.

## Direction

- **Colour: restrained.** System neutrals and materials everywhere; the system accent for
  selection and badges; colour lives in bubbles and chat colours. `Theme` shrinks to bubble and
  chat colours.
- **Scene:** the owner glancing at Hush between tasks on a MacBook, in whatever appearance macOS is
  set to. Light and dark are both first-class; the app follows the system (the Settings choice
  stays as an override).
- **References:** Messages.app (structure), Telegram for macOS (density, bubbles), Linear and
  Raycast (precision).
- **Glass** only on the navigation and control layer: the toolbar and sidebar (drawn by the
  system), the composer, and the "New messages" pill. Never on bubbles, notices, rows or results.

## Scope

Production, one PR. In: the window, sidebar, conversation header, composer, decryption notices,
error banner, appearance default, macOS 26 minimum. Out: bubble and timeline styling beyond the
notices, Settings, the call window, stories, and dropping duplicate deliveries silently (needs
presage; its own issue). `DESIGN.md` is written from the result.

## Layout

- **Window:** no app-painted backgrounds. `NavigationSplitView` with the system sidebar and the
  detail on the window background.
- **Sidebar:**
  - The chat list is a native `List(selection:)` with `.listStyle(.sidebar)`. Each row has the
    avatar, the name, a one-line preview and the time; the system draws the selection.
  - Search is `.searchable(placement: .sidebar)`.
  - Compose and Stories are sidebar toolbar items. The archive shows under an "Archived" heading,
    with a "Back to Chats" toolbar button.
  - No large "Chats" title.
  - Connection status is one quiet line at the foot of the sidebar, shown only while not online.
- **Header (Messages style):**
  - It lives in the window toolbar. The title is the chat's name. The subtitle shows, in order of
    priority: typing, offline/connecting in words, the disappearing timer, @username.
  - Trailing items: Call, and one info menu holding Disappearing Messages ▸, Chat Colour and
    Wallpaper…, Search in Chat (⇧⌘F), Group Info… in a group, and the note about them if there is
    one.
  - No header inside the content and no divider: the scroll-edge effect separates them.
- **Composer:**
  - It floats over the timeline (`safeAreaBar` at the bottom), and messages scroll under it.
  - A `GlassEffectContainer` holds a round glass "+" menu (Attach Files… ⇧⌘A, Stickers…) and the
    glass message field (continuous 18pt corners).
  - The reply and edit bars, the link preview and the file tray sit in the same container, above
    the field.
  - Emoji stays in Edit ▸ Emoji & Symbols (⌃⌘Space).
- **Decryption notices:**
  - Consecutive failures collapse into one centred row: "3 messages from He Be Hatin couldn't be
    decrypted" (or "A message…"), named by the chat's title in a direct chat.
  - The row has one small borderless "Reset Secure Session…" button. It asks first: "Hush starts
    a new encrypted session. He Be Hatin will see that it was reset. Messages that couldn't be
    decrypted stay unreadable; ask them to resend anything important."
  - After the tap it reads "Resetting…" and stays disabled until the reset is recorded.
  - Decryption failures no longer reach the error banner.
- **Error banner:** kept for other errors, in plain words, inside the sidebar under the toolbar.
  The icon is tinted, there's no full-bleed fill, and it never sits under the traffic lights.

## States

- **Chat list:** empty, searching, no results, archived.
- **Header:** typing, offline, group, disappearing timer.
- **Composer:** empty, up to six lines then scrolling, replying, editing, files attached, link
  preview.
- **Notice:** single, collapsed (N), resetting, reset.
- **Banner and connection:** banner shown and dismissed; connecting and offline.
- **System settings:** Reduce Transparency (the system's solid fallback), Increase Contrast,
  light and dark.

## Interaction

Keyboard behaviour is kept:
- ⌘K, ⌘F (focuses the sidebar search), ⇧⌘F, ⌥↑↓, ⌥⇧↑↓, ⌘1–9.
- ↩ sends, ⇧↩ breaks the line, ↑ in an empty field edits your last message.
- Esc ends search, reply and edit.

Risk: the native search field may take over the ↑↓↩ result browsing. Check it; keep the custom
handling (`onKeyPress`) if it does.

## Copy

- No enum names on screen, and names wherever they are known (a chat whose name isn't known yet
  still shows a short form of its id, as before).
- **Subtitles:** "typing…", "Offline", "Connecting…", "Disappearing messages: 1 day",
  "@username".
- **Notice and confirmation:** as written under Layout.

## As built

- **`Theme`** loses the window and sidebar fills from the main window. Its remaining greys
  (field, selection, secondary text) stay for in-content use; replacing them with system
  colours (and so honouring Increase Contrast) is left for a later pass.
- **The decryption notice** says "between you and …", not "from …": a contact's failure (a
  retry receipt) shows as ours does.
- **Stories:** a filled icon, not a number badge, marks unseen stories (a badge would clip in
  the toolbar); the count is in its label.
- **Search keys:** the ↑↓ handlers sit on both the list and the searchable container; ↩ is
  `onSubmit(of: .search)`; leaving the field empty (Esc, a click elsewhere) ends the search and
  its chat scope. Whether the system's search field passes ↑↓ on is checked in the running app.
- **Clicking a chat** opens it through the list's own selection and then puts the caret in its
  composer (the selection is reported with the mouse-up current). A tap gesture on the rows did
  this first, but it swallowed clicks on the picture, name and last message. So a click on the
  chat already open no longer moves the caret; Focus Composer (⇧⌘T) does.
- **Reset in progress** is the store's (`resettingSessions`): cleared when the reset is recorded
  or anything fails, so a failed reset is offered again.
- **The Core Log window** no longer lists decryption failures: the chat shows them, and the
  unified log keeps the sender id.
- **The Rust core** is still built for macOS 15 (`scripts/build-core.sh`): objects built for an
  older target link into the macOS 26 app without warnings, and a rebuild of the C dependencies
  isn't worth it.
- **Visual source of truth:** `DESIGN.md`.

## Constraints

- `deploymentTarget` becomes macOS 26.0 (`app/project.yml`); the stack rules in `CLAUDE.md` follow.
- Behaviour covered by tests stays green. New logic (collapsing notices, the subtitle) is
  test-first. The look is checked by the owner in the running app (this Mac can't take
  screenshots).
