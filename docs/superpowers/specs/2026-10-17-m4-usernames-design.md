# Usernames (M4 design)

## Context
Issue #52: "Usernames and username links: show, find someone by username." A contact's
username comes in its storage record (`ContactRecord.username`); presage resolves a username to
its ACI (`Manager::lookup_username`, an unidentified request).

## Goals
1. A contact's username shows under its name in the chat header ("@name.42"); a chat with no
   name yet is titled by it.
2. A "New chat by username" button opens a sheet: a username ("name.42", with or without "@")
   is looked up and its chat opens, ready to write in; "No one has this username." otherwise.

## Not in this slice
Username links (`signal.me/#eu/…`), our own username (setting or showing it).

## Design
- Migration 0017: `conversations.username`, written by the storage pull (`set_username`).
- `Command::LookupUsername` runs presage's lookup on the core thread (15 s); `Client
  .find_username` checks the form with Signal's own rules (libsignal's `Username::new`), asks, and on a hit stores
  the username and lists the chat now (`open_found` sets its time when it has no messages).
- App: `FindUsernameSheet`; `AppStore.findUsername` selects the chat or says why not.

## Testing
- records: a contact's username is read. db: stored and changed only when different; a found
  chat lists at once, an existing one keeps its place. client: malformed usernames are refused
  before any request.
- App: found → selected; no one → the words; refused → the core's reason; the title falls back
  to the username. Sheet and header checked from a snapshot.
