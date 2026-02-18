CREATE TABLE mls_group_states_v2 (
  user_id INTEGER NOT NULL,
  device_id TEXT NOT NULL,
  guild_id INTEGER NOT NULL,
  channel_id INTEGER NOT NULL,
  group_state_bytes BLOB NOT NULL,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  schema_version INTEGER NOT NULL DEFAULT 0,
  group_state_blob BLOB,
  key_material_blob BLOB,
  PRIMARY KEY (user_id, device_id, guild_id, channel_id)
);

INSERT INTO mls_group_states_v2 (
  user_id,
  device_id,
  guild_id,
  channel_id,
  group_state_bytes,
  updated_at,
  schema_version,
  group_state_blob,
  key_material_blob
)
SELECT
  0,
  'legacy',
  guild_id,
  channel_id,
  group_state_bytes,
  updated_at,
  schema_version,
  group_state_blob,
  key_material_blob
FROM mls_group_states;

DROP TABLE mls_group_states;
ALTER TABLE mls_group_states_v2 RENAME TO mls_group_states;
