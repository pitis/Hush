-- What a message is when it isn't text or files (a sticker, shared contact, payment, poll, gift
-- badge: shown as a placeholder), and its disappearing-message timer.
ALTER TABLE messages ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN kind_text TEXT;
-- seconds; 0 = doesn't disappear
ALTER TABLE messages ADD COLUMN expires_in INTEGER NOT NULL DEFAULT 0;
-- ms; set once the timer starts (when read, or sent by us)
ALTER TABLE messages ADD COLUMN expires_at INTEGER;
CREATE INDEX messages_expiring ON messages(expires_at) WHERE expires_at IS NOT NULL;
-- the chat's disappearing-message timer (seconds) and when it was last set
ALTER TABLE conversations ADD COLUMN expire_timer INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN timer_at INTEGER NOT NULL DEFAULT 0;
-- e.g. the new timer of a TimerChanged event
ALTER TABLE chat_events ADD COLUMN value INTEGER;
-- when a message was read before it reached us (its disappearing timer starts then)
ALTER TABLE pending_status ADD COLUMN read_at INTEGER;
-- presage copies of expired messages (and their edits) still to delete there; kept until done
CREATE TABLE presage_forget (
    conversation_id TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, sent_at)
) WITHOUT ROWID;
