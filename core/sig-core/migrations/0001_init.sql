-- Conversation ids: 'c:<aci uuid>' (direct) or 'g:<hex group master key>' (group); kind follows the prefix.
CREATE TABLE conversations (
  id TEXT PRIMARY KEY CHECK (substr(id, 1, 2) IN ('c:', 'g:')),
  kind INTEGER NOT NULL CHECK (kind = (substr(id, 1, 2) = 'g:')),  -- 0 direct, 1 group
  title TEXT NOT NULL DEFAULT '',
  last_message_at INTEGER NOT NULL DEFAULT 0,
  last_message_preview TEXT NOT NULL DEFAULT '',
  unread_count INTEGER NOT NULL DEFAULT 0 CHECK (unread_count >= 0)
);

-- A message is identified by (author, sent_at), as in the Signal protocol. Edits must be
-- `UPDATE … SET body`, never INSERT OR REPLACE (which bypasses the FTS triggers).
CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversations(id),
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  body TEXT,
  is_outgoing INTEGER NOT NULL CHECK (is_outgoing IN (0, 1)),
  status INTEGER NOT NULL CHECK (status BETWEEN -1 AND 3),  -- failed, pending, sent, delivered, read
  edited INTEGER NOT NULL DEFAULT 0 CHECK (edited IN (0, 1)),
  deleted INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
  quote_author TEXT,
  quote_sent_at INTEGER,
  quote_text TEXT,
  UNIQUE (author, sent_at),
  CHECK (deleted = 0 OR (body IS NULL AND quote_text IS NULL)),
  CHECK ((quote_author IS NULL) = (quote_sent_at IS NULL))
);
CREATE INDEX messages_by_conv ON messages(conversation_id, sent_at, author);
CREATE INDEX messages_unread ON messages(conversation_id) WHERE is_outgoing = 0 AND deleted = 0 AND status < 3;

CREATE TABLE reactions (
  target_author TEXT NOT NULL,
  target_sent_at INTEGER NOT NULL,
  reactor TEXT NOT NULL,
  emoji TEXT NOT NULL,
  PRIMARY KEY (target_author, target_sent_at, reactor)
);

CREATE VIRTUAL TABLE messages_fts USING fts5(body, content='messages', content_rowid='id');
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER messages_au AFTER UPDATE OF body ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
