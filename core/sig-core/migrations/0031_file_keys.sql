-- Files kept from chats are encrypted (`files.rs`). An attachment's and a chat picture's own key;
-- NULL for a file from before, plain until encrypted at launch under a new name. (Sticker images
-- and a draft's preview image are sealed under keys derived from `install_key`.)
ALTER TABLE attachments ADD COLUMN file_key BLOB;
ALTER TABLE conversations ADD COLUMN avatar_key BLOB;
-- a file read by its path finds its attachment
CREATE INDEX attachments_file ON attachments (file);
