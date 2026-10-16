-- Blocking and message requests: blocked/whitelisted are now written here too (see 0011).
-- In the phone's synced contact list (its address book): such a chat is never a message request.
ALTER TABLE conversations ADD COLUMN phone_contact INTEGER NOT NULL DEFAULT 0 CHECK (phone_contact IN (0, 1));
-- presage threads (a deleted chat's) still to clear there; kept until done
CREATE TABLE presage_forget_threads (
    conversation_id TEXT PRIMARY KEY
) WITHOUT ROWID;
