-- The sticker our unsent sticker message carries (it goes once sent), so Retry sends it again.
CREATE TABLE outgoing_stickers (
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  pack_id BLOB NOT NULL,
  pack_key BLOB NOT NULL,
  sticker_id INTEGER NOT NULL,
  PRIMARY KEY (author, sent_at)
);
