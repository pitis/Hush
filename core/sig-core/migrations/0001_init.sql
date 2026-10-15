CREATE TABLE conversations (
  id TEXT PRIMARY KEY,
  kind INTEGER NOT NULL,            -- 0 direct, 1 group
  title TEXT NOT NULL DEFAULT '',
  last_message_at INTEGER NOT NULL DEFAULT 0,
  last_message_preview TEXT NOT NULL DEFAULT '',
  unread_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversations(id),
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  body TEXT,
  is_outgoing INTEGER NOT NULL,
  status INTEGER NOT NULL,          -- -1 failed, 0 pending, 1 sent, 2 delivered, 3 read
  edited INTEGER NOT NULL DEFAULT 0,
  deleted INTEGER NOT NULL DEFAULT 0,
  quote_author TEXT,
  quote_sent_at INTEGER,
  quote_text TEXT,
  UNIQUE (author, sent_at)
);
CREATE INDEX messages_by_conv ON messages(conversation_id, sent_at);

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
