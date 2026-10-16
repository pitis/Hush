# Link previews on sent messages (M4 design)

## Context
Incoming previews show since #46-era work; ours went out bare. Issue #52. Signal's
`DataMessage.preview` carries `{url, title, description, image: AttachmentPointer}`; recipients
show it only when its `url` is in the message's text. Desktop fetches the linked page itself
when "Generate link previews" is on (an account setting the phone syncs in
`SyncMessage.Configuration.linkPreviews`).

## Goals
1. Typing a message with an https link shows, after a pause, a card above the composer with the
   page's title, description and image; ✕ leaves it off (and that link isn't fetched again in
   the chat).
2. The card goes with the message (text messages, not captions); the recipient sees the preview.
3. No page is fetched unless the phone's setting allows it; while unknown, none is.

## Design
- Settings: `link_previews` joins `Op::Settings`/`Settings` (stored like the others).
- `link_preview` module: `first_link` (as typed and parsed), `parse` (Open Graph `og:title`,
  `og:description`, `og:image`, else `<title>`; entities decoded; image resolved against the
  page, https only), `fetch` with its own reqwest client (system roots via rustls' platform
  verifier, ring provider as presage installs it): https only, redirects too (at most 5), a 10 s
  timeout, the page cut at 1 MB and HTML only, the image at most 2 MB and JPEG/PNG/WebP/GIF by
  its bytes.
- `Client.link_preview_for(text)` returns a `PreviewDraft` (its image saved to a file under the
  data folder); `send_text(.., preview)` keeps it only when its link is in the text, stores it
  (`link_previews` row, image as the message's position -1 file) and the send job uploads the
  image and sets `DataMessage.preview` (an image that won't upload is left off). A retry sends it
  again.
- App: the composer waits 700 ms after typing, asks the store, which fetches for the first link
  and drops a draft whose link is gone; `PreviewDraftCard` shows it.

## Testing
- link_preview: first link as typed, Open Graph and fallback, entities, relative and http images,
  broken HTML, image types; a live fetch of example.com (ignored by default, run by hand).
- db: a sent preview is stored with its image; settings carry the new key.
- send: the preview goes in the `DataMessage`. client: kept only with its link in the text; not
  fetched while the setting is unknown or off.
- App: the first link as typed; fetched for the link, sent with the message, then cleared; a
  dismissed or stale preview isn't sent. Card checked from a snapshot.
