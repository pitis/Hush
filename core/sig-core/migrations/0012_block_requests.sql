-- Blocking and message requests: blocked/whitelisted are now written here too (see 0011).
-- In the phone's synced contact list (its address book): such a chat is never a message request.
ALTER TABLE conversations ADD COLUMN phone_contact INTEGER NOT NULL DEFAULT 0 CHECK (phone_contact IN (0, 1));
-- presage threads (a deleted chat's) still to clear there; kept until done
CREATE TABLE presage_forget_threads (
    conversation_id TEXT PRIMARY KEY
) WITHOUT ROWID;
-- the read sync for this row reached our other devices; the receipt to the sender may still wait
-- (a message request's or blocked chat's waits until it is accepted)
ALTER TABLE unsent_receipts ADD COLUMN synced INTEGER NOT NULL DEFAULT 0 CHECK (synced IN (0, 1));
