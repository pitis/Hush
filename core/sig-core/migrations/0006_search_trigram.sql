-- Substring search (words inside words, Chinese/Japanese/Korean text without spaces) for queries
-- of three or more characters; the unicode61 index (messages_fts) matches whole words and
-- prefixes. Kept in step with messages like messages_fts.
CREATE VIRTUAL TABLE messages_trigram USING fts5(body, content='messages', content_rowid='id', tokenize='trigram');
CREATE TRIGGER messages_trigram_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_trigram(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER messages_trigram_au AFTER UPDATE OF body ON messages BEGIN
  INSERT INTO messages_trigram(messages_trigram, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO messages_trigram(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER messages_trigram_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_trigram(messages_trigram, rowid, body) VALUES ('delete', old.id, old.body);
END;
INSERT INTO messages_trigram(messages_trigram) VALUES ('rebuild');
