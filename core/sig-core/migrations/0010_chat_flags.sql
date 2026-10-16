-- Per-chat flags as the phone keeps them: seeded by the history import, set from the app.
ALTER TABLE conversations ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));
-- NULL: not pinned. Pinned chats list first, in ascending order.
ALTER TABLE conversations ADD COLUMN pinned_order INTEGER;
-- Milliseconds since the epoch; 0: not muted; 9223372036854775807 (i64::MAX): always.
ALTER TABLE conversations ADD COLUMN mute_until INTEGER NOT NULL DEFAULT 0 CHECK (mute_until >= 0);
ALTER TABLE conversations ADD COLUMN marked_unread INTEGER NOT NULL DEFAULT 0 CHECK (marked_unread IN (0, 1));
