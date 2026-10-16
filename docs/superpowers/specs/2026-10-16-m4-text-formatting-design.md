# Text formatting and mentions, shown (M4 design)

## Context
Issue #53 asks for mentions and text formatting (body ranges), rendered and sent. This slice
renders them; composing them on the Mac comes next. Until now, ranges were dropped everywhere: a
mention showed as an empty box (U+FFFC) and styled text as plain.

Signal's `BodyRange` (`SignalService.proto`, `backup.proto`): `start` and `length` in UTF-16
code units; either a style (`BOLD=1, ITALIC=2, SPOILER=3, STRIKETHROUGH=4, MONOSPACE=5`) or a
mention (`mentionAci`, string or 16-byte binary in live messages, bytes in the backup) covering a
U+FFFC placeholder in the text. Ranges come on `DataMessage.bodyRanges`, `Quote.bodyRanges`, and
the backup's `Text.bodyRanges` (message and quote).

## Goals
1. Bold, italic, strikethrough, monospace and spoilers show in message bubbles and quotes;
   mentions read "@name" in bold (a mention of us: "@You").
2. Spoilers are covered until the message is clicked (Desktop reveals one on click).
3. The chat list, notifications, reply/edit previews and Copy show mentions as "@name";
   one-line places show a hidden spoiler as "■■■■" (Copy keeps its text).
4. Live messages, edits, sync transcripts and imported history all keep their ranges.
5. A reply quotes the styled text with its ranges, so the quote reads the same on the other side.

## Non-goals (next slices of #53)
- Composing styles or mentions on the Mac (keyboard shortcuts, the @ picker). Until then a
  message with styles or mentions can't be edited on the Mac (the edit would strip them).
- Story ranges.

## Design
- Core: `BodyRange { start, length, style: TextStyle }`, `TextStyle` = the five styles or
  `Mention { aci }`. `Message.ranges`, `Quote.ranges`, `Conversation.last_message_ranges`
  (the preview's). Unknown styles and unreadable mentions are left out.
- Storage (migration 0013): `messages.body_ranges`, `messages.quote_ranges`, JSON, NULL when
  none. An edit replaces `body_ranges`; delete for everyone clears both; a quoted message's
  deletion clears `quote_ranges` with `quote_text`. Unreadable JSON reads as no ranges.
- Sending: a quote's ranges go out as `Quote.bodyRanges` (mentions as `mentionAci`).
- App (`MessageText`): ranges become an `AttributedString` with each run's styles under a key
  of ours; `MessageText.text` draws one `Text` piece per run in its font. Mentioned names are
  looked up like group authors' (`loadName`), for messages, the chat list and notifications.
  A range that doesn't fit the text is ignored, except a spoiler, which hides to the end of the
  text; a mention inside a hidden spoiler stays covered (in the bubble and in plain text). A
  range without a start starts at 0 (proto2's default).

## Testing
- projector: body, quote and edit ranges; NONE, unknown styles and bad mentions dropped.
- db: round trip, edit replaces, delete clears, chat-list ranges, history import keeps them.
- history import: message and quote ranges from the backup.
- send: a quote's ranges go out.
- App: styles overlap, UTF-16 offsets, mentions in place, spoilers hidden/revealed, chat-list
  line; rendering checked visually (styles, spoiler, monospace).
