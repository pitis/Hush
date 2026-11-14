-- We have written in this chat (Desktop's sent-message count): it accepts its message request
-- for good, so it stays when our messages expire. Until now our messages' rows were the count.
ALTER TABLE conversations ADD COLUMN has_sent INTEGER NOT NULL DEFAULT 0 CHECK (has_sent IN (0, 1));
UPDATE conversations SET has_sent = 1 WHERE id IN (SELECT conversation_id FROM messages WHERE is_outgoing = 1);
