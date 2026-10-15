# Native macOS Signal client — Rust core + Swift UI (Milestone 1 design)

## Context
Pitis wants to replace Signal Desktop (Electron) with a native Mac app for personal daily use. Goals: **lighter/faster** (idle RAM ~50–80 MB vs 400 MB+) and **real Mac feel**. Core in Rust, UI in Swift. End goal is full Desktop parity, decomposed into milestones (agreed):

- **M1 Daily driver** (this spec): QR link as secondary device, chat list, 1:1 + groups, text, attachments + voice-note playback, reactions/replies/edits/deletes, receipts, typing, notifications, search.
- M2 History sync on link (libsignal message-backup import).
- M3 Voice/video calls (RingRTC).
- M4 Long tail: stories, stickers, usernames, settings parity.

Personal, unofficial build; no "Signal" branding if ever distributed; AGPL-3.0 (inherited from presage).

Environment verified: arm64, macOS 26.6, Xcode 27, Rust 1.96. Missing: `protoc`, `xcodegen` (brew install).

## Architecture
New project at `~/development/sigmac/` (working name; own git repo, outside the vault).

```
sigmac/
  core/                 Rust workspace
    sig-core/           presage wrapper + projection DB + UniFFI API
  app/                  SwiftUI/AppKit app, project.yml (XcodeGen, no hand-edited pbxproj)
  scripts/build-core.sh cargo build → uniffi-bindgen Swift → SigCore.xcframework (arm64)
  docs/superpowers/specs/  spec lives here
```

**Rust core (`sig-core`)**
- Built on **presage** (+ `presage-store-sqlite`, SQLCipher), pinned to a git rev. CDSI feature off in M1 (avoids the BoringSSL/OpenSSL conflict; contacts come from phone sync).
- **Projection DB** (own SQLite tables, same encrypted file): `conversations`, `messages` (body, quote ref, edit history, deleted flag, status), `reactions`, `attachments`, FTS5 `messages_fts`. Presage `Content` events are projected in; UI queries only the projection (fast list/unread/search).
- **UniFFI API** (async + callback interface):
  - `Client::open(db_path, db_key)`, `link(device_name, on_qr)`, `start()`
  - `conversations()`, `messages(conv_id, before, limit)`, `search(q)`
  - `send_text(conv, body, quote?)`, `react`, `edit`, `delete_for_everyone`, `send_attachment(path)`, `mark_read`, `set_typing`
  - `EventListener.on_event(Event)` — MessageUpserted, ReactionChanged, ReceiptUpdated, Typing, ConversationUpdated, ConnectionState, Error
- Outgoing sends tracked as pending → sent → delivered → read; retries with backoff.

**Swift app**
- SwiftUI `NavigationSplitView`: sidebar chat list (search, unread badges), detail chat.
- Chat timeline: **AppKit `NSTableView` via `NSViewRepresentable`** with SwiftUI cells (`NSHostingView`) — SwiftUI lazy stacks get slow on long chats; this is the "faster" goal.
- `@Observable` `AppStore` on MainActor fed by the Rust event callback.
- Composer: multiline, ⌘↩/↩ send, drag-drop/paste attachments, reply/edit modes, emoji reactions via context menu.
- Mac integration: `UNUserNotificationCenter` with inline reply, dock badge, ⌘K quick switcher, standard menus/shortcuts, Quick Look for attachments, `AVPlayer` for voice notes.
- Keys: SQLCipher DB key generated on first run, stored in Keychain.

## Error handling
- Connection state banner (connecting/offline/unlinked). Unlink by primary → wipe local data screen.
- Protocol/server changes break presage occasionally → pin rev, surface errors in an in-app log view, update presage deliberately.
- Undecryptable messages shown as placeholder, never crash the receive loop.

## Testing
- Rust: unit tests for projection (Content fixtures → rows; reaction add/remove, edit replaces body, delete tombstones, quote resolution), FTS search, send-state transitions.
- Swift: XCTest for `AppStore` event reduction.
- E2E manual: link to Pitis's phone (ideally a secondary/test number first), checklist per M1 feature, compare against Signal Desktop side by side; measure idle RAM in Activity Monitor.

## Next steps
1. Spec reviewed by Pitis.
2. writing-plans skill → task-by-task implementation plan; first task is a spike: presage links to phone and prints incoming messages from Swift via UniFFI (proves the whole stack end to end before UI work).
