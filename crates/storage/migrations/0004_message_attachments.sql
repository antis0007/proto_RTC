ALTER TABLE messages ADD COLUMN attachment_file_id INTEGER REFERENCES files(id);
ALTER TABLE messages ADD COLUMN attachment_filename TEXT;
ALTER TABLE messages ADD COLUMN attachment_size_bytes INTEGER;
ALTER TABLE messages ADD COLUMN attachment_mime_type TEXT;

ALTER TABLE files ADD COLUMN filename TEXT;
ALTER TABLE files ADD COLUMN size_bytes INTEGER;
