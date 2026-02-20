CREATE TABLE IF NOT EXISTS user_devices (
  device_id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id),
  device_name TEXT NOT NULL,
  device_public_identity TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  is_revoked INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_user_devices_user_active
  ON user_devices (user_id, is_revoked, device_id DESC);

CREATE TABLE IF NOT EXISTS device_link_tokens (
  token_id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id),
  initiator_device_id INTEGER NOT NULL REFERENCES user_devices(device_id),
  target_device_pubkey TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  consumed_at TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE mls_key_packages ADD COLUMN device_id INTEGER REFERENCES user_devices(device_id);
CREATE INDEX IF NOT EXISTS idx_mls_key_packages_lookup
  ON mls_key_packages (guild_id, user_id, device_id, id DESC);

ALTER TABLE pending_welcomes ADD COLUMN target_device_id INTEGER REFERENCES user_devices(device_id);
CREATE INDEX IF NOT EXISTS idx_pending_welcomes_target_device
  ON pending_welcomes (guild_id, channel_id, user_id, target_device_id, id DESC)
  WHERE consumed_at IS NULL;
