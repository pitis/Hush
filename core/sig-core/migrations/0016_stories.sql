-- Stories (text, or one photo/video in `attachments` under the same author and sent_at), kept
-- for a day. `conversation_id` is the author's chat, the group's, or ours (c:<me>) for our own.
-- A text story's look: `background` is a JSON list of colours (one, or a gradient's).
CREATE TABLE stories (
    author TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    conversation_id TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    text TEXT,
    text_style INTEGER NOT NULL DEFAULT 0,
    text_foreground INTEGER,
    text_background INTEGER,
    background TEXT NOT NULL DEFAULT '[]',
    angle INTEGER NOT NULL DEFAULT 0,
    link TEXT,
    viewed INTEGER NOT NULL DEFAULT 0 CHECK (viewed IN (0, 1)),
    -- ours, posted from the phone: shown whatever its chat
    outgoing INTEGER NOT NULL DEFAULT 0 CHECK (outgoing IN (0, 1)),
    PRIMARY KEY (author, sent_at)
) WITHOUT ROWID;
