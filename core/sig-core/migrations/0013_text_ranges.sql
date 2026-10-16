-- Text styles and mentions (Signal's body ranges), as JSON lists of `BodyRange`; NULL: none.
ALTER TABLE messages ADD COLUMN body_ranges TEXT;
ALTER TABLE messages ADD COLUMN quote_ranges TEXT;
