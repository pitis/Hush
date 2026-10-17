# Chat pictures (M4 design)

## Context
Issue #54: "Real avatars in place of initials." presage fetches a contact's profile avatar with
their profile key (`retrieve_profile_avatar_by_uuid`) and a group's avatar with its master key
(`retrieve_group_avatar`), caching both in its store without ever re-checking (its own TODO).

## Goals
1. Chats show their picture (a contact's profile avatar, a group's avatar, ours for Note to
   Self) wherever their avatar shows; initials when there is none.
2. Pictures are fetched in the background, a few at a time, without holding up messages.

## Design
- Migration 0018: `conversations.avatar` (the picture's file) and `avatar_checked_at`.
- After catch-up and each contact sync, the core thread takes up to 20 listed chats not looked
  at in the last day (`avatars_to_fetch`, newest first) and fetches each (10 s each): a
  contact's with the profile key presage holds for them (ours for Note to Self; none known: no
  picture), a group's with its master key. `store_avatar` writes it beside the attachments
  (`avatar-<chat id>`), removes one taken away, and reports a change; a failed fetch only marks
  the chat looked at, and is tried again in a day. A change announces `ConversationsChanged`.
- App: `AvatarPictures.shared` (paths by chat id, filled with the chat list); `Avatar` draws the
  picture (downsampled, a circle) when there is one.

## Limits
presage never re-checks its cached profile or avatar, so a contact's new picture shows once
their profile key changes (clearing presage's cache would also drop the profile keys it sends
with). A group member who isn't a listed chat keeps initials.

## Testing
- db: listed chats are asked for once a day, newest first, as many as asked; a picture is kept
  as a file, unchanged ones don't announce, one taken away goes with its file.
- App: chats' pictures reach `AvatarPictures`. Avatars checked from a snapshot.
