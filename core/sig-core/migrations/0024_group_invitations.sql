-- We are invited to the group and haven't answered (as of `group_revision`).
ALTER TABLE conversations ADD COLUMN group_invited INTEGER NOT NULL DEFAULT 0;
-- Stored again with the flag at the next sync (a group message re-stores its group first, so
-- its members still judge its messages).
UPDATE conversations SET group_revision = NULL WHERE id LIKE 'g:%';
