-- What the phone calls a contact (the nickname set for them, else their address-book name) and
-- the note written about them, from its storage record. The name is preferred to the profile's.
ALTER TABLE conversations ADD COLUMN storage_name TEXT;
ALTER TABLE conversations ADD COLUMN note TEXT;
-- Records pulled before contact details were read are read once more for them (only for them:
-- a pending local change to a chat's settings stays).
ALTER TABLE storage_sync ADD COLUMN details_read INTEGER NOT NULL DEFAULT 0 CHECK (details_read IN (0, 1));
