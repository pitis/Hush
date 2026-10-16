# View-once media (M4 design)

## Context
Until now view-once messages were dropped. Issue #53: "View-once media: open once, then gone."
Desktop shows received view-once photos and videos once, can't send them, and syncs openings
with the phone (`SyncMessage.ViewOnceOpen{senderAci, senderAciBinary, timestamp}`).

## Goals
1. A received view-once photo or video shows as a "View photo"/"View video" button (or a
   download button until its file is here); opening it shows it in Quick Look once, then the
   bubble says "Viewed" and the file is gone.
2. Opening it here tells our phone; opening it on the phone does the same here.
3. Ours (sent from the phone) shows as "View-once media", not openable, and its file isn't
   fetched. The caption isn't kept (it belongs to the media).
4. Imported history shows view-once messages as viewed (their files don't come over).

## Design
- `MessageKind::ViewOnce { viewed }` (kind 6; `kind_text` "viewed" once opened).
- `Op::ViewOnceOpened { target }` (from our phone's sync, or from `Client.view_once_opened`):
  marks it viewed and deletes its attachments and files. Only an unviewed view-once message.
- `Client.view_once_opened(key)` applies it and queues `Job::ViewOnceOpen`, which sends
  `view_once_open_sync` to our devices.
- App: the bubble hides the attachment list for view-once media; the button links the file for
  Quick Look (`Files.openable`) before telling the core, so the viewer keeps it after the core
  deletes the original; the link goes when the preview closes.

## Not done
- Expiring unopened view-once media after a while (Desktop drops them after 30 days).
- An opening that arrives before its message is ignored.

## Testing
- projector: unviewed in, ours viewed with no attachments fetched, caption dropped; the phone's
  opening (and nobody else's) becomes `ViewOnceOpened`.
- db: viewed and files gone, once only, not for other messages.
- send: our opening, projected as our phone would, names the same message.
- client: opening applies here and queues the sync; refused twice or for a normal message.
- import: view-once comes in viewed.
- App: the button's states and words; opening asks the core; bubbles checked from a snapshot.
