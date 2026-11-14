-- Each contact's identity key as Hush last saw it in presage's store, which keeps only the
-- current one: a different key there is a changed safety number, shown in their chats.
CREATE TABLE identity_keys (
  aci TEXT PRIMARY KEY,
  key BLOB NOT NULL
) WITHOUT ROWID;
