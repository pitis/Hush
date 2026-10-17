-- We are invited to the group and haven't answered (as of `group_revision`).
ALTER TABLE conversations ADD COLUMN group_invited INTEGER NOT NULL DEFAULT 0;
