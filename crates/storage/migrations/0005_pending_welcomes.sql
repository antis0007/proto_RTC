CREATE TABLE IF NOT EXISTS pending_welcomes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  guild_id INTEGER NOT NULL REFERENCES guilds(id),
  channel_id INTEGER NOT NULL REFERENCES channels(id),
  user_id INTEGER NOT NULL REFERENCES users(id),
  welcome_bytes BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  consumed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_pending_welcomes_unconsumed_lookup
  ON pending_welcomes (guild_id, channel_id, user_id, id DESC)
  WHERE consumed_at IS NULL;
