# M1 Daily-Driver UI Implementation Plan (Plan 2a)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the linked, receiving spike into a usable Signal client: send messages, and a real chat list + conversation view + composer that look like Signal Desktop (owner's reference screenshot, 2026-10-15).

**Architecture:** Sending goes through the core actor (presage `send_message` / `send_message_to_group`) with an optimistic `Pending` row in the projection that moves to `Sent`/`Failed`. The Swift side replaces the spike `CoreBridge` with an `AppStore` (selection, per-conversation message windows, coalesced refreshes) driving a `NavigationSplitView`: sidebar list, conversation timeline (SwiftUI `ScrollView` + `LazyVStack`, bottom-anchored, paged), composer.

**Tech Stack:** as Plan 1. SwiftUI (macOS 15), `@Observable`.

**Spec:** `docs/superpowers/specs/2026-10-15-m1-daily-driver-design.md`

## Global Constraints
- Everything in Plan 1's constraints still holds (actor-only presage, no content in logs, signing, macOS 15/arm64).
- Visual reference: Signal Desktop dark theme. Outgoing bubble `#2C6BED` with white text, incoming `#3B3B3D`, window background `#1B1B1D`, sidebar selection `#2C2C2E`, corner radius 18, bubble max width 65 % of the timeline, time + ticks inside the bubble's trailing edge, avatars 44 pt in the sidebar (initials on a stable color until profile avatars land).
- Deviation from the spec, deliberate: the timeline starts as SwiftUI `LazyVStack` (bottom-anchored with `defaultScrollAnchor(.bottom)`); switch to `NSTableView` only if a 10 000-message chat measurably stutters (#12 acceptance).

## Review Focus
1. Sending before the first `QueueEmpty` (sessions not yet updated) must queue, not fail.
2. A send that fails must stay visible as Failed with retry, never vanish.
3. A message arriving for the open conversation must appear without scrolling the user away from where they are reading (only auto-scroll when already at the bottom).
4. Group messages show the sender's name; unknown senders fall back to a short id, never crash.
5. Switching conversations quickly must never show another conversation's messages (stale async results).

---

### Task 1 (#7): Send text with optimistic state
**Core**
- `Command::Send { key: MessageKey, conversation_id, body, quote: Option<Quote> }`; `Client::send_text(conversation_id, body, quote) -> Result<MessageKey, CoreError>` inserts an outgoing `Pending` row (author = own ACI, `sent_at` = now ms, unique) **before** queueing, returns the key; emits `MessageChanged`.
- Actor: waits for the first `QueueEmpty` (sends queue until then), builds `DataMessage { body, timestamp, quote }`, `c:<uuid>` → `send_message(ServiceId, …)`, `c:PNI:<uuid>` → PNI ServiceId, `g:<hex>` → `send_message_to_group(key, …)`. Success → `SetStatus Sent`; error or 60 s timeout → status `Failed` + `Error` event. presage stores its own copy in its store only; the server does not echo our own sends back to this device.
- `ProjectionDb::set_failed(key)` (only Pending → Failed). `Client::retry(key)` re-queues a Failed message (Failed → Pending).
- `Client::own_aci() -> Option<String>`.
- Tests: recipient parsing for all id forms + invalid ids; optimistic insert → Pending, preview updated, not unread; set_failed only from Pending; retry flips Failed → Pending; Pending rows left by a previous run become Failed at startup (a receipt still lifts them); a crashed sender is restarted and its in-flight message becomes Failed.

### Task 2 (#10): AppStore
- `@MainActor @Observable AppStore` (replaces `CoreBridge`; keeps its link/boot logic and tests): `conversations`, `selectedId`, `messages: [Message]` for the selection (newest window of 100, `loadOlder()` pages 100 more), `typing: [conversationId: Date]` expiring after 15 s, `send(text)`, `retry(key)`.
- Refreshes coalesced per kind (conversations; selected messages). A result for a conversation that is no longer selected is dropped (generation counter).
- Contact names for group senders: `Client::title(id)` → the stored conversation title of `c:<aci>` (title rows exist even when hidden from the list).
- Tests (FakeClient): selection loads messages; MessageChanged for the selection refreshes it, for another conversation only refreshes the list; stale selection result dropped; send inserts via client and refreshes; typing expiry.

### Task 3 (#11): Sidebar

> **As built (PR #31):** the list, search (⌘F), rows, relative times, unread badges and typing are done. Keyboard navigation, the ⌘K switcher, the dock badge, sidebar ticks/deleted preview and minute-refreshing times moved to #32.
>
> **As built (#32), after Signal Desktop:**
> - **Shortcuts** (Chats menu): ⌥↑/⌥↓ previous/next chat, wrapping around. ⌥⇧↑/⌥⇧↓ previous/next unread, without wrapping. ⌘1–9 jumps to a chat. ⌘F/⌘K search. ⌘⇧T focuses the composer. ⌘⇧C closes the chat.
> - **Search:** ↑/↓ pick a chat, ↩ opens it and moves to the composer, Esc ends the search.
> - **Rows:** groups show "Name: text" ("You: …" for our own). "You/Name deleted this message". "Draft: …" for unsent text in chats that aren't open. "Note to Self". Ticks for our own last message.
> - **Dock badge:** total unread messages.
> - **Read:** a chat is marked read when its newest message is visible in the key window.
> - **Read:** the window must also be on screen, so a locked or sleeping Mac doesn't mark incoming messages read.
> - **Focus:** chat shortcuts and clicking a chat move focus to the composer; ↑/↓ in the list or the search switch chats but keep focus there.
> - **⌥↑/⌥↓ clash:** like Desktop, they switch chats even in the composer, overriding "move to paragraph start/end".
> - **Not done:** ⌃Tab/⌃⇧Tab (Tab is taken by focus moves). ⌘↑/⌘↓ and ⌘J, because moving to the start or end of a long draft is used far more often than paragraph moves.
- `NavigationSplitView` sidebar (min 300 pt): "Chats" header, search field (filters by title, ⌘F focuses), rows = avatar (initials, stable color from id) · title (bold if unread) · preview (one line, "Photo"/"Attachment" when body nil, "This message was deleted" when deleted) · relative time (Now / 11m / 14:05 / Mon / Jul 18) · unread badge (blue capsule) · ticks for own last message.
- Keyboard: ↑/↓ moves selection, ⌘1–9 jumps, ⌘K quick switcher (fuzzy title).
- Dock badge = total unread.
- Tests: relative time formatter (all buckets, timezone Europe/Vienna in test), preview text rules, fuzzy matcher, avatar color stability.

### Task 4 (#12): Conversation view

> **As built (PR #31):**
> - **Scrolling:** at-bottom and near-top come from `onScrollGeometryChange` (pure decisions in `Timeline`, tested). Your own new message always scrolls into view. Timeline rows are computed only when messages change (`AppStore.timelineItems`), and the header is a separate view.
> - **Retry** is a button.
> - **Mark read when visible:** moved to #32 (needs #8).
- Header: avatar, title, connection subtitle if not online.
- Timeline: day separators ("Today", "Yesterday", "Mon 13 Oct"), bubbles grouped (same sender within 3 min → tight spacing, sender name shown in groups on first of run), quote block inside bubble, "edited" label, deleted → italic "This message was deleted", reactions as a pill under the bubble (emoji + count), time + ticks (pending clock, sent ✓, delivered ✓✓, read ✓✓ filled, failed ⚠ with "Retry" on click).
- Auto-scroll only when at bottom; "↓ new messages" pill otherwise. Older messages load when scrolled near the top, keeping position.
- Marks the conversation read locally when visible and window is key (core `mark_read` in #8 sends receipts; until then local only via `SetStatus Read` on incoming).
- Context menu: Reply, Copy text.
- Tests: grouping and day-separator computation (pure function over `[Message]`), tick state mapping, scroll decision (atBottom/new message → scroll or pill).

### Task 5 (#13): Composer

> **As built (PR #31):**
> - **Keys:** ↩ sends via `onKeyPress`, with `onSubmit` as a fallback. ⇧↩/⌥↩ are left to the field, so the newline goes at the cursor. ↩ while an input method is composing commits the composition and doesn't send.
> - **Failures:** the text is restored if the core can't take the message.
- Rounded multiline field (grows to 6 lines), placeholder "Message", ↩ sends / ⇧↩ newline, send disabled when empty/whitespace, reply banner (Esc cancels), emoji button opens the system character palette, draft per conversation kept in memory and `UserDefaults`.
- Tests: send/newline key handling decision, trimming, draft save/restore per conversation, reply state cleared after send.

### Task 6 (new issue): Import history from Signal Desktop
Separate spec section + plan when Tasks 1–5 land: read `Signal Safe Storage` from the Keychain (owner approves the prompt), decrypt Desktop's `config.json` `encryptedKey`, open `~/Library/Application Support/Signal/sql/db.sqlite` read-only with SQLCipher, map `conversations` + `messages` (json) into the projection idempotently.
