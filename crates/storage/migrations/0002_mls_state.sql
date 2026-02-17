CREATE TABLE IF NOT EXISTS mls_identity_keys (
  user_id INTEGER NOT NULL,
  device_id TEXT NOT NULL,
  identity_bytes BLOB NOT NULL,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, device_id)
);

CREATE TABLE IF NOT EXISTS mls_group_states (
  guild_id INTEGER NOT NULL,
  channel_id INTEGER NOT NULL,
  group_state_bytes BLOB NOT NULL,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (guild_id, channel_id)
);
