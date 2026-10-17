# Mentioning people (M4 design)

## Context
#58 shows mentions and #60 writes styles. This slice writes mentions: in a group, "@" and the
start of a name offer its members; the one picked goes out as Signal's mention (a U+FFFC
placeholder under a `mentionAci` range). Messages with mentions can be edited again.

## Goals
1. In a group chat, typing "@" (at the start or after a space) and letters shows the members
   whose name has a word starting with them, by name, us left out. ↑/↓ choose, ↩ or Tab picks,
   a click picks, Esc closes it until the next "@word". Nothing in a direct chat.
2. A picked member shows as "@Name" in the accent colour, followed by a space; typing into it
   makes it plain text again. Styles around a mention apply to its placeholder.
3. Editing a message with mentions loads them back as tokens.

## Design
- Core (migration 0014): `group_members(conversation_id, aci)`, replaced for each group at title
  sync from presage's group store; `Client.group_members(id)`. Membership changes reach it at
  the next sync (start, contact sync).
- App: `ComposerText` keeps a token as "@Name" under `hush.mention` (the ACI) and
  `hush.mentionText` (the text inserted); `message(from:)` shrinks each token to U+FFFC
  before reading styles and adds the mention ranges. `MentionPicker` follows the "@word" before
  the caret and inserts through the text view (undoable). Names come from contact titles
  (`AppStore.mentionCandidates`).

## Testing
- db: members replaced whole.
- App: query detection, insertion, styles around a token, a changed token, the edit round trip,
  name matching; the picker in a real window (open, ↓, ↩ picks without sending, Esc closes and
  ↩ then sends); `mentionCandidates` leaves us out and names members; editing allowed.
