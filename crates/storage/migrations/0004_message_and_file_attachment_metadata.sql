ALTER TABLE messages ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE files ADD COLUMN file_name TEXT NOT NULL DEFAULT 'file.bin';
ALTER TABLE files ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE files ADD COLUMN mime_type_v2 TEXT;
UPDATE files SET mime_type_v2 = COALESCE(mime_type, 'application/octet-stream');
ALTER TABLE files DROP COLUMN mime_type;
ALTER TABLE files RENAME COLUMN mime_type_v2 TO mime_type;
