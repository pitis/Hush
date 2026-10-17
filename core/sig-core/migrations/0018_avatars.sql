-- A chat's picture (a contact's profile avatar or a group's), as a file path, and when it was
-- last looked for (so a chat without one isn't asked about again for a day).
ALTER TABLE conversations ADD COLUMN avatar TEXT;
ALTER TABLE conversations ADD COLUMN avatar_checked_at INTEGER NOT NULL DEFAULT 0;
