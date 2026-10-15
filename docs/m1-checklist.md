# M1 sign-off: Semaphore next to Signal Desktop

**Merge #42 before the week starts:** it fixes message loss on reconnect and unlinking from #41.

Run both side by side for a week (#17). For each row, do the check in Semaphore and compare
with Signal Desktop. `make release` installs the app; `make perf` measures the budget.

Legend: ✅ built (the PR that did it) · ◐ partly · ✗ not in M1 (where it goes)

## Linking and connection

| Feature | Status | Check |
|---|---|---|
| Link as a secondary device with a QR code | ✅ #27, #28 | Fresh install → scan → chats appear |
| Contact and group names | ✅ #26 | Names match the phone |
| Connection banner (Connecting / Offline + Retry) | ✅ #41 | Wi-Fi off → banner; Retry |
| Reconnect when the network returns (< 3 s) | ✅ #41 + #42 | Wi-Fi off/on, time the banner |
| Removed on the phone → link again without restarting | ✅ #41, #42 | Phone ▸ Linked devices ▸ remove → Link Again |
| Unlink this Mac (Settings) | ✅ #41 | Settings ▸ Unlink This Mac… |
| Undecryptable message notice + session reset | ✅ #41, #42 | — (rare; seen after session problems) |
| History from before linking | ✗ M2 (#18) | New messages only |

## Chats

| Feature | Status | Check |
|---|---|---|
| Chat list: unread badges, ticks, typing, drafts, "You deleted…", group "Name: …" | ✅ #31, #35 | Compare rows with Desktop |
| Note to Self | ✅ #35 | Your own chat is named Note to Self |
| Keyboard: ⌥↑/⌥↓, ⌥⇧↑/⌥⇧↓, ⌘1–9, ⌘⇧T, ⌘⇧C | ✅ #35 | Chats menu |
| Dock badge = unread messages | ✅ #35 | Matches the sum of badges |
| Mark read when seen (and on the phone, with receipts) | ✅ #33, #35 | Open a chat → phone clears it |
| Read receipts / typing indicators follow the phone's settings | ✅ #33, #34 | Turn off on the phone → none sent |
| Pin, archive, mute, mark unread | ✗ M4 (#20) | |
| Message requests, blocking | ✗ M4 (#20) | |
| Profile photos (avatars are initials) | ✗ M4 (#20) | |

## Messages

| Feature | Status | Check |
|---|---|---|
| Send text, 1:1 and groups, with pending/sent/delivered/read ticks | ✅ #29, #33 | Send both ways |
| Failed send with Retry | ✅ #29 | Send offline |
| Reply (quote) | ✅ #29, #31 | Context menu ▸ Reply |
| Reactions in and out: hover picker, menu, "More…", who reacted | ✅ #25, #33, #43 | React, change, remove; the phone shows it |
| Edit (menu, ↑ in an empty composer) and delete for everyone, in and out | ✅ #25, #33, #43 | Edit and delete; the phone shows "edited" / deleted |
| Delete for me | ✗ (owner's decision, #43) | Desktop syncs it to your devices |
| Day separators, grouping, "edited", deleted | ✅ #31 | |
| Stickers, shared contacts, payments, polls, gift badges | ◐ #24 | Placeholder rows ("🙂 Sticker message", "📊 Poll: …"), counted as unread; the content itself is M4 |
| Disappearing messages | ✅ #24 | Set a timer on the phone: the header shows it, messages go when it runs out (incoming: after being read) |
| View-once media, stories | ✗ M4 (#20) | |

## Attachments

| Feature | Status | Check |
|---|---|---|
| Photos inline, Quick Look | ✅ #38, #39 | Receive a photo |
| Files: download on request, Retry when failed | ✅ #38, #39 | Receive a PDF |
| Voice notes and audio playback | ✅ #39 | Receive a voice note |
| Link previews (received) | ✅ #38, #39 | Receive a link |
| Send files: paperclip, drag and drop, paste, with caption | ✅ #38, #39 | Send a photo and a PDF |
| Record voice notes, send link previews, GIFs/stickers picker | ✗ M4 (#20) | |
| Attachments encrypted on disk | ✗ owner's decision (asked in #38) | Stored as plain files |

## Search, notifications, Mac

| Feature | Status | Check |
|---|---|---|
| Search chats and messages (⌘F), in a chat (⌘⇧F), jump to result | ✅ #40 | Search an old word |
| Notifications with Reply and Mark as Read; content setting | ✅ #36 | App in background, receive |
| No burst of notifications after being offline | ✅ #36 | Offline 1 h, reconnect |
| Reaction notifications | ✅ #36 | React to your message from the phone |
| Keeps running with the window closed; menu bar item with unread | ✅ #44 | Close the window; dock click and the menu bar reopen it |
| Open at login | ✅ #44 | Settings ▸ Open at login |
| Core Log window | ✅ #41 | Window ▸ Core Log |
| Calls | ✗ M3 (#19) | |

## Budget (#17)

Record `make perf` output on the issue.

| | Budget | Measured |
|---|---|---|
| App size | < 40 MB | 33 MB (Release, arm64) |
| Cold launch to chat list | < 1 s | `make perf` |
| Idle memory | < 100 MB | `make perf` |
| Idle CPU | ~0 % | `make perf` |
