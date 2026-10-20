-- The installed sticker packs as the picker shows them and sending needs them, so that presage's
-- copy (every image in it as JSON) is read only until the pack is here: a pack is kept once every
-- image of it is written to stickers/<hex pack id>/<sticker id>.
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
