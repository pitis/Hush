-- One in-flight history import (link-and-sync): resumed on launch, deleted when done or
-- abandoned. `key` is the one-time ephemeral backup key from the provisioning message.
CREATE TABLE history_import (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  stage TEXT NOT NULL CHECK (stage IN ('waiting', 'downloading', 'importing')),
  key BLOB NOT NULL CHECK (length(key) = 32),
  aci TEXT NOT NULL,
  cdn INTEGER,
  cdn_key TEXT,
  bytes_done INTEGER NOT NULL DEFAULT 0,
  total_bytes INTEGER
);
