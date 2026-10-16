-- Each group's members (from presage's group store, at title sync), for the mention picker.
CREATE TABLE group_members (
    conversation_id TEXT NOT NULL,
    aci TEXT NOT NULL,
    PRIMARY KEY (conversation_id, aci)
) WITHOUT ROWID;
