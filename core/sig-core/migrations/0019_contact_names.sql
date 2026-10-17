-- What the phone calls a contact (the nickname set for them, else their address-book name) and
-- the note written about them, from its storage record. The name is preferred to the profile's.
ALTER TABLE conversations ADD COLUMN storage_name TEXT;
ALTER TABLE conversations ADD COLUMN note TEXT;
