-- The revision of presage's group the chat's title, members and timer were last read at; NULL:
-- never (contacts, and groups from before this).
ALTER TABLE conversations ADD COLUMN group_revision INTEGER;
