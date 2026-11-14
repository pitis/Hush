-- A random key of this install's (written on open): a chat's opaque id, in logs, file names and
-- notifications, is its keyed hash of the chat id, which names no one without this database.
CREATE TABLE install_key (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  key BLOB NOT NULL
);
-- What was typed in a chat and not sent yet (the words, not their styles).
CREATE TABLE drafts (
  conversation_id TEXT PRIMARY KEY,
  text TEXT NOT NULL
);
