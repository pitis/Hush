# Blocking and message requests (M4 design)

## Context
#50 reads each chat's `blocked` and `whitelisted` (profile sharing, i.e. "message request
accepted") from the Storage Service, with no UI. This slice uses them: block and unblock from the
Mac, hold chats from unknown senders back as message requests, and drop what blocked senders
send. Issue #51.

What exists (verified 2026-10-16 on the pinned revisions):
- presage has **no block list** and filters nothing: every decrypted content is saved to
  presage.db, then handed to us as `Received::Content`. `SyncMessage.Blocked` and
  `SyncMessage.MessageRequestResponse` pass through untouched.
- Protos (`presage::libsignal_service::proto`): `sync_message::Blocked { numbers, acis,
  group_ids, acis_binary }` (`group_ids` are GV2 group **identifiers**, derived from the master
  key, not master keys); `sync_message::MessageRequestResponse { thread_aci, group_id, r#type,
  thread_aci_binary }`, types `Accept=1, Delete=2, Block=3, BlockAndDelete=4, Spam=5,
  BlockAndSpam=6`; `data_message::Flags::ProfileKeyUpdate = 4`.
- presage's `send_message` injects our profile key into every outgoing `DataMessage`. A sync
  message to our own ACI goes to our other devices only (how `Job::StorageChanged` already
  sends `FetchLatest`).
- presage cannot leave a GV2 group (libsignal-service builds group-change actions but has no
  `PATCH /v2/groups`).
- `ContentsStore::clear_thread` removes a thread from presage.db.

Signal Desktop's rules (main, `ts/util/isConversationAccepted`, `MessageReceiver`,
`models/conversations` `applyMessageRequestResponse`, `jobs/helpers/shouldSendToConversation`):
- A chat is accepted when it is Note to Self, has profile sharing on, is a system contact, or we
  have sent in it (an empty 1:1 counts as accepted). Requests sit **inline** in the inbox; the
  preview line says "Message request" or "Blocked"; the composer is replaced by the request bar.
- Blocked senders: every content is dropped right after decryption (data, typing, receipts,
  calls). Blocked groups: data messages, edits and stories are dropped.
- Accept: unblock, profile sharing on, read receipts for what was read meanwhile, our profile
  key to the contact (1:1), `MessageRequestResponse(ACCEPT)` to our devices (1:1).
- Block: block list + profile sharing off, `MessageRequestResponse(BLOCK)` (1:1), the full
  `Blocked` list to our devices, leaves the group (local action only). Unblock = Accept.
- Delete: profile sharing off, messages destroyed, `MessageRequestResponse(DELETE)` (1:1).
- From the phone: `Blocked` **replaces** the list (added → block, removed → accept);
  `MessageRequestResponse` applies the same change without sending anything back.
- No receipts to a chat that is a request or blocked; no profile key either.
- Storage records carry `blocked` and `whitelisted = profileSharing`, written on every change.

## Decisions (owner, 2026-10-16)
- Blocking a group **blocks without leaving** (leaving needs the group-change API: later). The
  Mac drops the group's messages and tells the phone.
- A chat counts as a request only once the Mac has read the account's state from the phone
  (a Storage Service pull completed at least once). Before that, every chat is accepted.
- Delete on a 1:1 request deletes on the phone too (`MessageRequestResponse(DELETE)`); a group's
  is local, as on Desktop.

## Goals
1. A chat from someone we haven't accepted shows as a request: "Message request" in the list,
   a bar with **Accept**, **Delete** and **Block** instead of the composer, no read receipts.
2. Block and unblock a person or a group from the Mac (chat list menu and the bar); the phone
   and other devices follow; blocked chats show "Blocked" and a bar with **Unblock** and
   **Delete**.
3. Nothing a blocked sender or group sends reaches the projection, a notification, or the call
   UI, and its copy in presage.db is removed.
4. Blocks, unblocks, accepts and deletes made on the phone apply on the Mac.

## Non-goals
- Leaving a group; group invites (pending membership); Report spam.
- Blocking by phone number only (`numbers` in the list are kept by the phone, ignored here).
- Rotating the profile key on block (the phone does it).
- System contacts as "trusted" (the Mac has none; the phone's `whitelisted` stands for them).

## Rules

### What counts as a request (core)
`Conversation` gains `message_request: bool`, true when all hold:
- the Storage Service state is known (`storage_sync.version > 0`);
- not `whitelisted`, not `blocked`;
- no outgoing message in the chat (ours from this Mac, synced from the phone, or imported
  history — Note to Self always has them).

A chat with no messages isn't listed, so the empty-chat rule needs nothing.

### Dropping (core, receive loop)
Right after `Received::Content`, before calls and the projection, for content not from our
own ACI:
- the sender's `c:<aci>` is blocked → drop;
- a `DataMessage`/`EditMessage` (or typing) whose group (`g:<master key>`) is blocked → drop.

A dropped data or edit message's presage copy is queued in `presage_forget` (conversation,
timestamp) and removed by the existing sweep. Logged as a count with the kind only.

### Actions (core `Client`)
| Call | Local change | Sent to our devices | Sent to others |
|---|---|---|---|
| `accept_request(id)` | `whitelisted=1, blocked=0` | 1:1: MRR `ACCEPT`; if it was blocked: `Blocked` list | 1:1: our profile key (`DataMessage{flags: PROFILE_KEY_UPDATE}`); the held read receipts |
| `block(id)` | `blocked=1, whitelisted=0` | 1:1: MRR `BLOCK`; `Blocked` list | — |
| `unblock(id)` | as `accept_request` | as `accept_request` | as `accept_request` |
| `delete_chat(id)` | `whitelisted=0`; messages, files, events, search rows gone; presage thread cleared | 1:1: MRR `DELETE` | — |

Every change marks the chat `storage_dirty` and triggers a storage sync, so the record follows
(when the chat has one; when it doesn't, the `Blocked` list and the MRR tell the phone, which
writes its own). The `Blocked` list is built at send time from the projection: blocked ACIs
(string and binary) and the group identifiers of blocked groups (from their master keys). Note
to Self can't be blocked or deleted this way (`InvalidInput`).

Sends ride the existing send queue as new jobs (`RequestResponse`, `BlockedList`,
`ProfileKey`): failures are logged and reported like `StorageChanged`'s, not retried.

### Receipts
`unsent_receipts` leaves out messages of chats that are requests or blocked: receipts queued
while reading a request wait, and go out once it's accepted (Desktop sends them on accept).
Typing needs nothing: the composer isn't there.

### From the phone (projector + db)
- `Blocked`: the list replaces ours. Each listed ACI or group (matched by identifier against
  our group chats' master keys) not blocked here → `blocked=1, whitelisted=0`; each chat blocked
  here and missing from the list → `blocked=0, whitelisted=1`. An ACI we have no chat for gets
  a row (not listed until a message), as storage pull does. Not marked dirty: the phone wrote
  its records.
- `MessageRequestResponse` (thread ACI or group identifier): `ACCEPT` → accept;
  `BLOCK`/`BLOCK_AND_SPAM` → block; `DELETE` → delete chat; `BLOCK_AND_DELETE` → both; `SPAM`
  → nothing. Not dirty, nothing sent back.

### Storage Service
`records::patch_chat` now owns `blocked` and `whitelisted` too (ContactRecord 9/10,
GroupV2Record 2/3); `clear_dirty` compares all five flags. Everything else in #50 stands.

### App
- Chat list: the second line reads "Message request" or "Blocked" for those chats. Menu:
  "Block…"/"Unblock" (not for Note to Self).
- Conversation: the composer is replaced by a bar. Request: "<name> wants to message you." (a
  group: "Join this group? They won't know you've seen their messages until you accept.") with
  Block, Delete, Accept. Blocked: "You blocked <name>." with Delete, Unblock. Block and Delete
  ask for confirmation first. No call button in either.
- Notifications as now (Desktop notifies for requests); blocked senders never reach it.

## Data (migration 0012)
- `presage_forget_threads (conversation_id TEXT PRIMARY KEY)`: presage threads to clear,
  processed by the sweep next to `presage_forget`, so a deleted chat's copies go even if
  clearing fails once.

## Testing
- db: `message_request` true/false across whitelisted, blocked, outgoing present, storage state
  unknown; `unsent_receipts` skips requests and blocked chats, and returns them after accept;
  `delete_chat` removes messages, events, files and queues the thread; block/accept set flags and
  dirty; the `Blocked` list replace (added, removed, unknown ACI, group by identifier); MRR types.
- records: the patcher writes blocked/whitelisted for contacts and groups, keeps unknown fields.
- receive loop: content from a blocked sender (data, typing, call) and a blocked group's data
  never reaches the projection or the call queue, and is queued for forgetting; our own sync
  messages pass.
- send: the MRR and `Blocked` sync messages and the profile-key message are built as above.
- App: `ChatList.line` for requests and blocked chats; the bar's buttons call the store.
- Real account (owner's check list in the PR): block on the Mac → phone shows blocked; unblock
  on the phone → Mac; a request from a new number → accept on the Mac → phone shows accepted.
