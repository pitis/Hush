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
  (format, creation time, the copied database's key; kept in memory on restore, never written
  out), `projection.db`, `attachments/<name>`. Restore writes only those names (no path escapes).
- A backup is written to `<path>.part` and moved over `path` when done: a failure keeps the
  backup it was replacing.
- The projection is copied with `sqlcipher_export` into a SQLCipher file under a fresh random
  key (`ProjectionDb::export_to`); the pool's single connection makes the copy consistent.
  Nothing is written in clear on disk.
- Restore (`Client.restore_backup`, unlinked only): decrypt into `restore.tmp/`, open and migrate
  the copy. It isn't trusted: its schema must equal a fresh projection's (no extra triggers or
  views) and its attachment names be plain; chat pictures not among its files are dropped. Drop
  its pending history import (the old link's key) and export it under this Mac's key. Every
  failure so far is `InvalidInput`: nothing changed, the core and its code stay.
- Then `StopUnlinked`: the core stops only if still unlinked (a scan meanwhile keeps the link
  and nothing is restored); presage's store and sticker copies go; a `ready` mark is written;
  `projection.db` (its old `-wal`/`-shm` removed first) and `attachments/` are renamed in. Every
  step can be repeated: an open that finds the mark finishes the swap, and otherwise removes the
  work folders. The app restarts the core keeping its key; the new link screen says how it went
  (`AppStore.restoreNote`).
- Linking can now be stopped while the code waits for a scan (`actor::race_link`): the actor
  races the link against its commands, ends on `StopUnlinked`/`Relink`/`Unlink` or a closed
  channel, and queues the rest for after the link.

## Not done
- Presage's store (sessions, keys) isn't in the backup: a restored Mac links as a new device.
- Restoring a backup of one account and linking another mixes their chats, as relinking to
  another phone already does.
- No progress bar: a large backup shows a spinner until done. The copy holds the projection's
  only connection meanwhile, so the core's database work waits for it (it fails after 30 s).
- Stopping right after a scan, while the link finishes, leaves that new device on the account
  (the phone lists it under Linked Devices).

## Testing
- backup.rs: round trip over several chunks; nothing readable in clear; wrong passphrase vs
  another file vs cut-off, flipped byte or trailing bytes; names outside the folder; the
  manifest first; absurd key parameters refused before hashing.
- client: a backup restores on another data folder with another key (chats, files, no `.part`
  copies, no pending import, core stopped) and replaces what was there (a live journal
  included); a wrong passphrase, an extra trigger, an escaping file name, a newer format, a
  missing or wrong key each keep everything and the core running, with their reason; a cut-short
  swap finishes at the next open and leftovers without the mark go; a failed backup keeps the
  file it was replacing; a short passphrase and a restore while linked are refused.
- actor: a code waiting for a scan stops for a restore, relink or unlink, ends when the Client
  goes, and queues other commands; a finished link loses none.
- db: an export is ciphertext, has the same schema, and refuses a malformed key.
- App: the backup reaches the core and a failure says why; a restore keeps the key and asks for
  a new code; a wrong passphrase keeps the code; a failure after the core stopped starts over
  and the note survives the new code.

## Export (second PR)
- A chat's right-click menu ▸ Export chat… asks for a folder, writes a new folder there named
  after the chat (`Name`, `Name 2`, …: an earlier export is never overwritten), and shows it in
  the Finder.
- `Client.export_chat` (`export.rs` renders): `chat.html`, a self-contained page (inline style,
  light and dark) with day headings, each message's time, author ("You" for ours, contact names
  otherwise), quote, "edited", and its files; mentions read "@name"; deleted messages say so
  and show nothing of themselves; stickers, polls and the like say what they were. Every text is
  HTML-escaped. Downloaded files are copied to `files/<id>-<name>` (pictures inline, the rest as
  links); the others say "not downloaded".
- The folder is built hidden (`.Name.partial-<pid>`) and named when done, the name claimed
  with an atomic `mkdir`: a failure leaves nothing. A file gone from disk reads "file missing"
  and the rest exports; names lose `/ : # ? %` and are cut at 200 bytes.
- View-once media is never exported (only its note), nor a deleted message's files.
- Not exported: reactions, text styles, chat events (calls, timer changes), link previews.
- Disappearing messages still in the chat are exported like any other: the export doesn't
  disappear.
- Testing: export.rs renders escaped text, days and times, mentions and quotes, deletions, files
  and other kinds, and safe folder names; the client exports a chat with a copy of its file and
  a second export beside the first; an unknown chat is refused; view-once media stays out; a
  missing file is said and the rest exports; a failed export leaves nothing; 1203 messages
  export whole; the app shows a failure.
