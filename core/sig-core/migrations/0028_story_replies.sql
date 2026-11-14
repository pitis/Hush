-- A reply or reaction to a story, shown in its 1:1 chat: the story's author and sent time
-- (the story itself goes after a day).
ALTER TABLE messages ADD COLUMN story_author TEXT;
ALTER TABLE messages ADD COLUMN story_sent_at INTEGER;
