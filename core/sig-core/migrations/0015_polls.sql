-- A poll's options and state, keyed by its message; `options` is a JSON list of strings (empty
-- until the poll itself is here, when its end arrived first).
CREATE TABLE polls (
    author TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    options TEXT NOT NULL DEFAULT '[]',
    allow_multiple INTEGER NOT NULL DEFAULT 0 CHECK (allow_multiple IN (0, 1)),
    ended INTEGER NOT NULL DEFAULT 0 CHECK (ended IN (0, 1)),
    PRIMARY KEY (author, sent_at)
) WITHOUT ROWID;
-- Each voter's latest vote (the highest vote_count wins); `options` is a JSON list of option
-- indexes, empty when the vote was taken back.
CREATE TABLE poll_votes (
    target_author TEXT NOT NULL,
    target_sent_at INTEGER NOT NULL,
    voter TEXT NOT NULL,
    vote_count INTEGER NOT NULL,
    options TEXT NOT NULL,
    PRIMARY KEY (target_author, target_sent_at, voter)
) WITHOUT ROWID;
