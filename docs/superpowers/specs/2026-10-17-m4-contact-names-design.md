# Contacts' names and notes from the phone (M4 design)

## Context
Issue #54: "Contact names, nicknames and notes from storage records (a non-goal of #50)." A
`ContactRecord` carries the nickname set for the contact (`nickname`), their address-book name
(`systemGivenName`/`systemFamilyName`/`systemNickname`) and a private note (`note`). Signal
names a contact by nickname, then address-book name, then profile name.

## Goals
1. A contact's chat (and everywhere their name shows: group senders, notifications, mentions)
   uses the phone's name for them when it has one, else the profile's as before; taking it away
   on the phone brings the profile's back.
2. The note shows in the chat header (an icon; the note as its tooltip).

## Design
- Migration 0019: `conversations.storage_name`, `conversations.note`, written by the storage
  pull (`set_contact_details`; the name is the nickname, else the address-book name).
- `conversations()` and `title(id)` read `COALESCE(storage_name, title)`; the profile's name
  stays in `title`, untouched.

## Testing
- records: nickname first, else the address-book name; none when both are empty; the note.
- db: the phone's name wins in the list and in `title`, unchanged details don't report, taking
  them away brings the profile's name back.
