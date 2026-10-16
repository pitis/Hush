# Viewing stories (M4 design)

## Context
Stories were dropped. Issue #52: "Stories: view, then post." This slice views them. A
`StoryMessage` is a text story (`TextAttachment`: text, style, colours, a gradient or colour
background, an optional link) or one photo or video (`fileAttachment`), sent to a contact list
or a group (`group`); its id is the envelope's timestamp. Our own come from the phone as
`SyncMessage.Sent.storyMessage`.

## Goals
1. A Stories button in the sidebar (with the count of unseen stories) opens a window: chats with
   stories on the left (ours first, unseen next, newest first), the story on the right; ←/→
   step through them, into the next chat's; Esc closes.
2. Text stories show their words in their style and colours on their background; photo stories
   show their image (downloaded like a message's), video stories play; a pending or failed file
   offers Download/Retry.
3. Shown stories are marked seen here. Stories last a day.
4. Only stories from chats that aren't blocked or message requests show (as Desktop), and ours.

## Not in this slice
Posting, replies and reactions to stories, viewed receipts to the author, story privacy
settings, hiding someone's stories.

## Design
- Migration 0016: `stories` (author, sent_at, conversation_id, text and its look, link, viewed,
  outgoing). A photo or video is the story's `attachments` row (same author and sent_at):
  `conversation_of` falls back to stories, so it is listed, downloaded and announced like a
  message's file.
- `Op::InsertStory` (→ `CoreEvent::StoriesChanged`); `ProjectionDb::stories(now)` (a day, the
  chat filter, each with its file); `story_viewed`. `delete_expired` drops stories older than a
  day with their files and queues presage's copies; deleting a chat takes its stories.
- App: `StoryList` (grouping, unseen count, ARGB colours), `StoriesView`, `StoryCard`; the store
  reloads stories on `StoriesChanged`, catch-up and a story file's change, and names authors.

## Testing
- projector: text (style, gradient), a group photo story with its file, ours from the phone.
- db: shown for a day from accepted chats only, with its file (which downloads), seen, expired
  with its file, gone with a deleted chat; a blocked chat's hidden; ours shown without a chat.
- App: grouping and order, the unseen count, colours, loading and marking seen; the window
  checked from a snapshot.
