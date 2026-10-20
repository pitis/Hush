-- The installed sticker packs as the picker shows them and sending needs them. presage keeps
-- each pack with every image in it as JSON, so its copy is read once, when the pack is first
-- listed: a sticker is here once its image is written to stickers/<pack id>/<sticker id>.
CREATE TABLE sticker_packs (
    id BLOB PRIMARY KEY NOT NULL,
    key BLOB NOT NULL,
    title TEXT NOT NULL,
    author TEXT NOT NULL
);

CREATE TABLE stickers (
    pack_id BLOB NOT NULL REFERENCES sticker_packs (id) ON DELETE CASCADE,
    id INTEGER NOT NULL,
    emoji TEXT,
    -- presage's, else empty
    content_type TEXT NOT NULL,
    PRIMARY KEY (pack_id, id)
);
