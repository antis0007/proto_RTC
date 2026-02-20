ALTER TABLE device_link_tokens ADD COLUMN token_secret TEXT;
ALTER TABLE device_link_tokens ADD COLUMN source_signature_public_key TEXT;

UPDATE device_link_tokens
SET token_secret = hex(randomblob(16))
WHERE token_secret IS NULL;

CREATE TABLE IF NOT EXISTS device_link_bundles (
  token_id INTEGER PRIMARY KEY REFERENCES device_link_tokens(token_id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL,
  source_device_id INTEGER NOT NULL REFERENCES user_devices(device_id),
  target_device_id INTEGER NOT NULL REFERENCES user_devices(device_id),
  bundle_json TEXT NOT NULL,
  uploaded_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  consumed_at TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_device_link_bundles_user ON device_link_bundles(user_id, token_id);
