-- Time (the edit's own sent timestamp) of the edit currently shown; older edits arriving later are ignored.
ALTER TABLE messages ADD COLUMN edited_at INTEGER NOT NULL DEFAULT 0;
