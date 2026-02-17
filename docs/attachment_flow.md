# Attachment Flow Design + Migration Notes

## UX behavior
1. User clicks **Attach** in the desktop composer and picks a file.
2. File is read locally and queued in the composer (10MB client-side guard).
3. On send, client uploads each queued file to `/files/upload` with `file_name` and `content-type`.
4. After upload(s) succeed, client sends the message via `/messages` with:
   - encrypted `ciphertext_b64` for text content,
   - `attachments[]` metadata (`file_id`, `file_name`, `mime_type`, `size_bytes`).
5. Chat history/events carry the same attachment metadata in `MessagePayload`.
6. Clicking an attachment in GUI downloads via `/files/:file_id?user_id=...` and prompts a save path.

## Protocol/API compatibility
- `MessagePayload.attachments` is optional (`default` + omitted when empty) so text-only behavior is unchanged.
- Old clients that ignore unknown JSON fields continue to parse/send text-only messages.
- New server accepts missing `attachments` in send-message requests via `#[serde(default)]`.
- File download now requires `user_id` query membership validation.

## Storage migration
- Added `messages.attachments_json` (`TEXT NOT NULL DEFAULT '[]'`).
- Added `files.file_name` and `files.size_bytes` with defaults for existing rows.
- Rebuilt `files.mime_type` as non-null via temporary column copy.
- Migration is additive for message path and keeps existing ciphertext rows valid.

## Reused/prior-attempt logic status
- Existing `/files/upload` and `/files/:file_id` endpoints were retained and extended.
- Existing `ServerEvent::FileStored` remained untouched for protocol compatibility.
- Existing text-message flow (`ciphertext_b64`) is unchanged; attachments are additive metadata.
- No recoverable old PR branch was available in this checkout; porting used current in-repo implementation as baseline.
