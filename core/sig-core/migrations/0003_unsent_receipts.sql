-- Messages marked read here whose read receipts (and read sync to the phone) haven't gone out yet.
CREATE TABLE unsent_receipts (
  author TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (author, sent_at)
);
