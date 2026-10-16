-- Storage Service sync: the manifest we applied, its items, and what is waiting to be written.
CREATE TABLE storage_sync (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL DEFAULT 0,
  record_ikm BLOB NOT NULL DEFAULT X'',
  -- SHA-256 of the storage key the state belongs to: a new key means a fresh pull.
  key_fingerprint BLOB NOT NULL DEFAULT X'',
  -- A pin change not yet written.
  account_dirty INTEGER NOT NULL DEFAULT 0 CHECK (account_dirty IN (0, 1)),
  -- Pinned-list entries we couldn't map, as raw PinnedConversation bytes: u32 LE length || bytes…
  unmappable_pins BLOB NOT NULL DEFAULT X''
);
-- Every identifier of the applied manifest: its 16-byte key, ManifestRecord.Identifier.Type,
-- and the chat it maps to when we understood the record.
CREATE TABLE storage_items (
  key BLOB PRIMARY KEY CHECK (length(key) = 16),
  kind INTEGER NOT NULL,
  conversation_id TEXT REFERENCES conversations(id)
);
-- As the phone says; read-only here.
ALTER TABLE conversations ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1));
ALTER TABLE conversations ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0 CHECK (whitelisted IN (0, 1));
-- A local flag change not yet written to the Storage Service.
ALTER TABLE conversations ADD COLUMN storage_dirty INTEGER NOT NULL DEFAULT 0 CHECK (storage_dirty IN (0, 1));
