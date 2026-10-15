-- Files of messages. `position` orders them within the message; -1 is the link preview's image.
-- `pointer` is the encoded AttachmentPointer to download from (NULL for our own until uploaded).
-- `file` is the file's name in the attachments folder once it is there.
CREATE TABLE attachments (
  id INTEGER PRIMARY KEY,
  message_author TEXT NOT NULL,
  message_sent_at INTEGER NOT NULL,
  position INTEGER NOT NULL,
  content_type TEXT NOT NULL,
  file_name TEXT,
  size INTEGER NOT NULL,
  width INTEGER,
  height INTEGER,
  is_voice_note INTEGER NOT NULL DEFAULT 0,
  pointer BLOB,
  file TEXT,
  state INTEGER NOT NULL,
  UNIQUE (message_author, message_sent_at, position)
);

CREATE TABLE link_previews (
  message_author TEXT NOT NULL,
  message_sent_at INTEGER NOT NULL,
  url TEXT NOT NULL,
  title TEXT,
  description TEXT,
  PRIMARY KEY (message_author, message_sent_at)
);
