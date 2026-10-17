-- The chat's wallpaper (`ChatColor::to_row`: a solid colour or gradient); NULL: the default.
-- The default (`app_values.default_wallpaper`) is the phone's, from history.
ALTER TABLE conversations ADD COLUMN wallpaper TEXT;
