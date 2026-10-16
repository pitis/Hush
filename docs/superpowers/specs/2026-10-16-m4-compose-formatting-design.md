# Writing styled text (M4 design)

## Context
#58 shows Signal's text styles and mentions. This slice lets the Mac write the styles: bold,
italic, strikethrough, monospace and spoiler. Mentions (the @ picker) are the next slice; until
then a message with mentions still can't be edited here.

## Goals
1. Desktop's shortcuts style the selection, or what's typed next when nothing is selected:
   ⌘B bold, ⌘I italic, ⇧⌘X strikethrough, ⌘E monospace, ⇧⌘B spoiler. The field's context menu
   has the same under Format. Undo takes a style back.
2. A sent message, a caption and an edit carry their ranges; editing loads a message's styles
   back into the field.
3. Everything the old field did still works: ↩ sends, ⇧↩/⌥↩ break the line, ↑ in an empty field
   edits our last message, ⌘V of files or a screenshot attaches them, up to six lines then
   scrolling, the emoji palette, focus on chat switch, drafts.

## Design
- App: the SwiftUI `TextField` gives way to `ComposerField`, an `NSTextView` in an
  `NSViewRepresentable` (SwiftUI's rich text editing needs macOS 26). Each character's styles
  sit under our own attribute and are drawn as fonts, a strikethrough or a spoiler's shade
  (`ComposerText`). Pasted text comes in plain: other apps' fonts and colours stay out.
- `ComposerText.message(from:)` trims the text and turns the styled runs into ranges (UTF-16,
  touching runs of one style joined).
- Drafts keep the words, not their styles.
- Core: `send_text`, `send_attachments` and `edit` take `ranges`; ranges that don't fit the text
  are dropped, and a caption's follow its trimming. `SendJob` carries them into
  `DataMessage.bodyRanges`; a retry sends the stored ones.

## Testing
- Core: a sent text's and caption's ranges are stored (and dropped when they don't fit, moved
  by trimming); the sent `DataMessage` carries them; our own edit's ranges apply locally.
- App: `ComposerText` (toggle, overlap, trimming, edit round trip, fonts, shortcuts) and
  `ComposerField` in a real window (typing, shortcuts and undo, ↩, ↑, growing to six lines,
  text set from outside); the field's look checked from a snapshot.
