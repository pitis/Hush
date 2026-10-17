# Sending stickers (M4 design)

## Context
Issue #52 asks for sending stickers. Received ones already show (#68). presage keeps sticker
packs with their images: it installs a pack when our phone says it installed one, and
`install_sticker_pack` downloads one and tells our other devices.

## Goals
1. A sticker button by the composer opens the installed packs, a tab each. A click sends the
   sticker to the open chat, as a reply if one is set.
2. Packs reach this Mac three ways:
   - The phone installs one: presage handles it already.
   - Linking with history: the archive lists the phone's packs, and they're installed once
     receiving runs.
   - Pasting a pack's link (`https://signal.art/addstickers/#pack_id=…&pack_key=…` or `sgnl://`)
     in the picker. This covers a Mac linked before this change.

## Design
- **Listing:** `Client::sticker_packs` reads presage's packs on the actor thread and writes each
  sticker's image once to `stickers/<pack id>/<sticker id>`. Only stickers with an image show.
  Packs are sorted by title.
- **Sending:**
  - `send_sticker` sends the image as the message's one file, through the normal file path:
    a pending row and a stored copy.
  - The message kind is Sticker, and `SendJob.sticker` puts the uploaded pointer in
    `DataMessage.sticker`, not in its attachments.
  - The pack and sticker id stay in `outgoing_stickers` until the message is sent, so Retry sends
    a sticker again.
- **History:** `Seed.sticker_packs` collects the archive's StickerPack frames, and
  `Outcome::Done` carries them. After `Start`, they're installed one by one on the history task.
  A pack that fails is logged and skipped.

## Not in this slice
- Removing packs, which the phone does.
- Sticker suggestions while typing an emoji.
- Animated stickers play only as far as the image view does.

## Testing
- Core: link parsing (both forms, rejects others); pack listing writes images once and leaves out
  stickers without one; a sticker DataMessage carries the image in the sticker; the importer keeps
  sticker packs; an unsent sticker keeps its sticker until forgotten.
- App: the picker loads packs; a sticker goes to the open chat with the reply; a failure keeps the
  reply and says so; a link adds the pack or says why. Picker layout checked from snapshots.
