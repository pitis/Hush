-- Our bubbles' colour in a chat (`ChatColor::to_row`); NULL: the default.
ALTER TABLE conversations ADD COLUMN chat_color TEXT;
-- Values that aren't on/off settings: `default_chat_color` (the phone's, from history).
CREATE TABLE app_values (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
