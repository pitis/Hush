-- Time (the edit's own sent timestamp) of the edit currently shown; older edits arriving later are ignored.
ALTER TABLE messages ADD COLUMN edited_at INTEGER NOT NULL DEFAULT 0;

-- After an edit, Signal clients target the message by the latest revision's timestamp (edits,
-- deletes, reactions, receipts). Maps every revision timestamp to the original one.
CREATE TABLE message_revisions (
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  original_sent_at INTEGER NOT NULL,
  PRIMARY KEY (author, sent_at)
);

-- Ops that arrived before their message; applied when the message is inserted.
CREATE TABLE pending_deletes (
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (author, sent_at)
);
CREATE TABLE pending_status (
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  status INTEGER NOT NULL,
  PRIMARY KEY (author, sent_at)
);
