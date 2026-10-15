-- Account settings synced from the phone (`read_receipts`, `typing_indicators`): 1 on, 0 off,
-- absent while unknown.
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value INTEGER NOT NULL
);
