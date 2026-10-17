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
- After catch-up and each contact sync, a task beside the receiving (one pass at a time, never
  holding up messages, sending or `Ready`) takes up to 20 listed, accepted chats (not blocked,
  not message requests: a stranger chooses that picture) not looked at in the last day (`avatars_to_fetch`, newest first) and fetches each (10 s each): a
  contact's with the profile key presage holds for them (ours for Note to Self; none known: no
  picture), a group's with its master key. `store_avatar` writes a new picture to a new file
  beside the attachments (`avatar-<chat id>-<time>`: the app caches pictures by path), removes
  the old one, and reports a change; a failed fetch only marks
  the chat looked at, and is tried again in a day. A change announces `ConversationsChanged`.
- App: `AvatarPictures.shared` (paths by chat id, filled with the chat list, none for blocked
  chats and requests); `Avatar` draws the picture (downsampled, a circle) when there is one.

## Limits
presage never re-checks its cached profiles and avatars: a contact's new picture shows once
their profile key changes (clearing presage's cache would also drop the profile keys it sends
with), and a group's changed or removed picture doesn't show at all (presage keys its cache by
the group alone); fixing that needs presage's group store, a later change. A group member who
isn't a listed chat keeps initials.

## Testing
- db: listed chats are asked for once a day, newest first, as many as asked; a picture is kept
  as a file, unchanged ones don't announce, one taken away goes with its file.
- App: chats' pictures reach `AvatarPictures`. Avatars checked from a snapshot.
