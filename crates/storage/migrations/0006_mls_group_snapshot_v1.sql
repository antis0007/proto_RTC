ALTER TABLE mls_group_states ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mls_group_states ADD COLUMN group_state_blob BLOB;
ALTER TABLE mls_group_states ADD COLUMN key_material_blob BLOB;
