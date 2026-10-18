# Backup and restore (M4 design)

## Context
Issue #56: "back up and restore the local store so history survives a re-link or a new Mac."
A re-link already keeps the projection (`Client.relink`), so the missing case is a new Mac: the
projection (`projection.db`, under a Keychain key that stays on the old Mac) and its files
(`attachments/`). The owner chose an encrypted backup plus, separately, a plain per-chat export.
This covers the backup; the export is its own PR.

## Goals
1. Settings ▸ Back Up… writes every chat and file here to one `.hushbackup` file, encrypted
   with a passphrase (at least 8 characters, typed twice).
2. The link screen's Restore from a Backup… replaces the chats here with the backup's, before
   linking; then linking goes on as after a relink (the phone's history merges in).
3. A wrong passphrase or a damaged or cut-off file is told apart and changes nothing.

## Design
- Format (`backup.rs`): header `HUSHBAK1 | salt | Argon2id params`, then AES-256-GCM chunks of
  1 MiB, each nonce its number plus a last-chunk flag and the header as associated data, so a
  reordered, cut-off or extended file fails. Inside: named entries, `manifest.json` first
  (format, creation time, the copied database's key), `projection.db`, `attachments/<name>`.
  Restore writes only those names (no path escapes).
- The projection is copied with `sqlcipher_export` into a SQLCipher file under a fresh random
  key (`ProjectionDb::export_to`); the pool's single connection makes the copy consistent.
  Nothing is written in clear on disk.
- Restore (`Client.restore_backup`, unlinked only): decrypt into `restore.tmp/`, open and migrate
  the copy, drop its pending history import (the old link's key), export it under this Mac's
  key; only then stop the core (`relink`: presage's store and sticker copies go), swap in
  `projection.db` and `attachments/`. The app restarts the core keeping its key; the new link
  screen says how it went (`AppStore.restoreNote`).
- Linking can now be stopped while the code waits for a scan: the actor races the link against
  its commands, ends on `Relink`/`Unlink` or a closed channel, and queues the rest for later.

## Not done
- Presage's store (sessions, keys) isn't in the backup: a restored Mac links as a new device.
- Restoring a backup of one account and linking another mixes their chats, as relinking to
  another phone already does.
- No progress bar: a large backup shows a spinner until done.

## Testing
- backup.rs: round trip over several chunks; nothing readable in clear; wrong passphrase vs
  another file vs cut-off, flipped byte or trailing bytes; names outside the folder; the
  manifest first; absurd key parameters refused before hashing.
- client: a backup restores on another data folder with another key (chats, files, no `.part`
  copies, no pending import, core stopped); a wrong passphrase keeps everything and the core
  running; a short passphrase and a restore while linked are refused.
- App: the backup reaches the core and a failure says why; a restore keeps the key and asks for
  a new code; a wrong passphrase keeps the code; a failure after the core stopped starts over
  and the note survives the new code.
