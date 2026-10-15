-- Things that happened in a chat that aren't messages, shown in its timeline (for now: a message
-- that couldn't be decrypted). `at` is when, in ms; `author` who it concerns.
CREATE TABLE chat_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  conversation_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  author TEXT NOT NULL
);
CREATE INDEX chat_events_by_conv ON chat_events(conversation_id, at);
