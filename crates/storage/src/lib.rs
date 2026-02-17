use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mls::MlsStore;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Pool, Sqlite,
};
use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use shared::domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId, Role, UserId};
use shared::protocol::AttachmentMetadata;

#[derive(Clone)]
pub struct Storage {
    pool: Pool<Sqlite>,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub message_id: MessageId,
    pub channel_id: ChannelId,
    pub sender_id: UserId,
    pub ciphertext: Vec<u8>,
    pub attachments: Vec<AttachmentMetadata>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoredFile {
    pub file_id: FileId,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub ciphertext: Vec<u8>,
    pub mime_type: String,
    pub file_name: String,
    pub size_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct StoredMember {
    pub user_id: UserId,
    pub username: String,
    pub role: Role,
    pub muted: bool,
}

impl Storage {
    pub async fn new(database_url: &str) -> Result<Self> {
        ensure_sqlite_parent_dir_exists(database_url)?;

        let connect_options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(connect_options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    pub async fn create_user(&self, username: &str) -> Result<UserId> {
        let rec = sqlx::query(
            "INSERT INTO users (username) VALUES (?)
             ON CONFLICT(username) DO UPDATE SET username=excluded.username
             RETURNING id",
        )
        .bind(username)
        .fetch_one(&self.pool)
        .await?;
        Ok(UserId(rec.get::<i64, _>(0)))
    }

    pub async fn username_for_user(&self, user_id: UserId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT username FROM users WHERE id = ?")
            .bind(user_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    pub async fn create_guild(&self, name: &str, owner_user_id: UserId) -> Result<GuildId> {
        let rec =
            sqlx::query("INSERT INTO guilds (name, owner_user_id) VALUES (?, ?) RETURNING id")
                .bind(name)
                .bind(owner_user_id.0)
                .fetch_one(&self.pool)
                .await?;
        let guild_id = GuildId(rec.get::<i64, _>(0));
        self.add_membership(guild_id, owner_user_id, Role::Owner, false, false)
            .await?;
        Ok(guild_id)
    }

    pub async fn create_channel(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
    ) -> Result<ChannelId> {
        let rec = sqlx::query(
            "INSERT INTO channels (guild_id, name, kind) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(guild_id.0)
        .bind(name)
        .bind(match kind {
            ChannelKind::Text => "text",
            ChannelKind::Voice => "voice",
        })
        .fetch_one(&self.pool)
        .await?;
        Ok(ChannelId(rec.get::<i64, _>(0)))
    }

    pub async fn add_membership(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role: Role,
        banned: bool,
        muted: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO memberships (guild_id, user_id, role, banned, muted)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET role=excluded.role, banned=excluded.banned, muted=excluded.muted",
        )
        .bind(guild_id.0)
        .bind(user_id.0)
        .bind(match role { Role::Owner => "owner", Role::Mod => "mod", Role::Member => "member" })
        .bind(banned)
        .bind(muted)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_guilds_for_user(&self, user_id: UserId) -> Result<Vec<(GuildId, String)>> {
        let rows = sqlx::query(
            "SELECT g.id, g.name
             FROM guilds g
             INNER JOIN memberships m ON m.guild_id = g.id
             WHERE m.user_id = ? AND m.banned = 0",
        )
        .bind(user_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (GuildId(r.get::<i64, _>(0)), r.get::<String, _>(1)))
            .collect())
    }

    pub async fn list_channels_for_guild(
        &self,
        guild_id: GuildId,
    ) -> Result<Vec<(ChannelId, String, ChannelKind)>> {
        let rows = sqlx::query("SELECT id, name, kind FROM channels WHERE guild_id = ?")
            .bind(guild_id.0)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let kind = match r.get::<String, _>(2).as_str() {
                    "voice" => ChannelKind::Voice,
                    _ => ChannelKind::Text,
                };
                (ChannelId(r.get::<i64, _>(0)), r.get::<String, _>(1), kind)
            })
            .collect())
    }

    pub async fn membership_status(
        &self,
        guild_id: GuildId,
        user_id: UserId,
    ) -> Result<Option<(Role, bool, bool)>> {
        let row = sqlx::query(
            "SELECT role, banned, muted FROM memberships WHERE guild_id = ? AND user_id = ?",
        )
        .bind(guild_id.0)
        .bind(user_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let role = match r.get::<String, _>(0).as_str() {
                "owner" => Role::Owner,
                "mod" => Role::Mod,
                _ => Role::Member,
            };
            (role, r.get::<bool, _>(1), r.get::<bool, _>(2))
        }))
    }

    pub async fn list_members_for_guild(&self, guild_id: GuildId) -> Result<Vec<StoredMember>> {
        let rows = sqlx::query(
            "SELECT u.id, u.username, m.role, m.muted
             FROM memberships m
             INNER JOIN users u ON u.id = m.user_id
             WHERE m.guild_id = ? AND m.banned = 0
             ORDER BY lower(u.username) ASC",
        )
        .bind(guild_id.0)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let role = match r.get::<String, _>(2).as_str() {
                    "owner" => Role::Owner,
                    "mod" => Role::Mod,
                    _ => Role::Member,
                };
                StoredMember {
                    user_id: UserId(r.get::<i64, _>(0)),
                    username: r.get::<String, _>(1),
                    role,
                    muted: r.get::<bool, _>(3),
                }
            })
            .collect())
    }

    pub async fn insert_message_ciphertext(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        ciphertext: &[u8],
        attachments: &[AttachmentMetadata],
    ) -> Result<MessageId> {
        let attachments_json = serde_json::to_string(attachments)?;
        let rec = sqlx::query(
            "INSERT INTO messages (channel_id, sender_user_id, ciphertext, attachments_json) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(channel_id.0)
        .bind(sender_id.0)
        .bind(ciphertext)
        .bind(attachments_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(MessageId(rec.get::<i64, _>(0)))
    }

    pub async fn guild_for_channel(&self, channel_id: ChannelId) -> Result<Option<GuildId>> {
        let row = sqlx::query("SELECT guild_id FROM channels WHERE id = ?")
            .bind(channel_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| GuildId(r.get::<i64, _>(0))))
    }

    pub async fn list_channel_messages(
        &self,
        channel_id: ChannelId,
        limit: u32,
        before: Option<i64>,
    ) -> Result<Vec<StoredMessage>> {
        let mut rows = if let Some(before_id) = before {
            sqlx::query(
                "SELECT id, channel_id, sender_user_id, ciphertext, attachments_json, created_at
                 FROM messages
                 WHERE channel_id = ? AND id < ?
                 ORDER BY id DESC
                 LIMIT ?",
            )
            .bind(channel_id.0)
            .bind(before_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, channel_id, sender_user_id, ciphertext, attachments_json, created_at
                 FROM messages
                 WHERE channel_id = ?
                 ORDER BY id DESC
                 LIMIT ?",
            )
            .bind(channel_id.0)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|r| {
                let attachments_json = r.get::<Option<String>, _>(4).unwrap_or_default();
                let attachments =
                    serde_json::from_str::<Vec<AttachmentMetadata>>(&attachments_json)
                        .unwrap_or_default();
                StoredMessage {
                    message_id: MessageId(r.get::<i64, _>(0)),
                    channel_id: ChannelId(r.get::<i64, _>(1)),
                    sender_id: UserId(r.get::<i64, _>(2)),
                    ciphertext: r.get::<Vec<u8>, _>(3),
                    attachments,
                    created_at: r.get::<DateTime<Utc>, _>(5),
                }
            })
            .collect())
    }

    pub async fn store_file_ciphertext(
        &self,
        uploader_id: UserId,
        guild_id: GuildId,
        channel_id: ChannelId,
        ciphertext: &[u8],
        mime_type: &str,
        file_name: &str,
        size_bytes: i64,
    ) -> Result<FileId> {
        let rec = sqlx::query(
            "INSERT INTO files (uploader_user_id, guild_id, channel_id, ciphertext, mime_type, file_name, size_bytes) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(uploader_id.0)
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(ciphertext)
        .bind(mime_type)
        .bind(file_name)
        .bind(size_bytes)
        .fetch_one(&self.pool)
        .await?;
        Ok(FileId(rec.get::<i64, _>(0)))
    }

    pub async fn load_file(&self, file_id: FileId) -> Result<Option<StoredFile>> {
        let row = sqlx::query(
            "SELECT id, guild_id, channel_id, ciphertext, mime_type, file_name, size_bytes FROM files WHERE id = ?",
        )
        .bind(file_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| StoredFile {
            file_id: FileId(r.get::<i64, _>(0)),
            guild_id: GuildId(r.get::<i64, _>(1)),
            channel_id: ChannelId(r.get::<i64, _>(2)),
            ciphertext: r.get::<Vec<u8>, _>(3),
            mime_type: r.get::<String, _>(4),
            file_name: r.get::<String, _>(5),
            size_bytes: r.get::<i64, _>(6),
        }))
    }

    pub async fn insert_key_package(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        key_package_bytes: &[u8],
    ) -> Result<i64> {
        let rec = sqlx::query(
            "INSERT INTO mls_key_packages (guild_id, user_id, key_package_bytes)
             VALUES (?, ?, ?)
             RETURNING id",
        )
        .bind(guild_id.0)
        .bind(user_id.0)
        .bind(key_package_bytes)
        .fetch_one(&self.pool)
        .await?;
        Ok(rec.get::<i64, _>(0))
    }

    pub async fn load_latest_key_package(
        &self,
        guild_id: GuildId,
        user_id: UserId,
    ) -> Result<Option<(i64, Vec<u8>)>> {
        let row = sqlx::query(
            "SELECT id, key_package_bytes
             FROM mls_key_packages
             WHERE guild_id = ? AND user_id = ?
             ORDER BY id DESC
             LIMIT 1",
        )
        .bind(guild_id.0)
        .bind(user_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| (r.get::<i64, _>(0), r.get::<Vec<u8>, _>(1))))
    }
}

fn ensure_sqlite_parent_dir_exists(database_url: &str) -> Result<()> {
    let Some(path) = sqlite_path(database_url) else {
        return Ok(());
    };

    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create parent directory '{}' for database url '{database_url}'",
            parent.display()
        )
    })?;

    Ok(())
}

fn sqlite_path(database_url: &str) -> Option<PathBuf> {
    if database_url == "sqlite::memory:" || !database_url.starts_with("sqlite:") {
        return None;
    }

    let path = database_url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:")
        .split('?')
        .next()
        .unwrap_or_default();

    if path.is_empty() {
        return None;
    }

    Some(Path::new(path).to_path_buf())
}

use sqlx::Row;

#[async_trait]
impl MlsStore for Storage {
    async fn save_identity_keys(
        &self,
        user_id: i64,
        device_id: &str,
        identity_bytes: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO mls_identity_keys (user_id, device_id, identity_bytes, updated_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(user_id, device_id) DO UPDATE SET identity_bytes = excluded.identity_bytes, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(identity_bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_identity_keys(&self, user_id: i64, device_id: &str) -> Result<Option<Vec<u8>>> {
        let row = sqlx::query(
            "SELECT identity_bytes FROM mls_identity_keys WHERE user_id = ? AND device_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>(0)))
    }

    async fn save_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        group_state_bytes: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO mls_group_states (guild_id, channel_id, group_state_bytes, updated_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(guild_id, channel_id) DO UPDATE SET group_state_bytes = excluded.group_state_bytes, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(group_state_bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<Vec<u8>>> {
        let row = sqlx::query(
            "SELECT group_state_bytes FROM mls_group_states WHERE guild_id = ? AND channel_id = ?",
        )
        .bind(guild_id.0)
        .bind(channel_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_and_lists_guilds() {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("alice").await.expect("user");
        let guild = storage.create_guild("devs", user).await.expect("guild");
        let guilds = storage
            .list_guilds_for_user(user)
            .await
            .expect("guild list");
        assert_eq!(guilds.len(), 1);
        assert_eq!(guilds[0].0, guild);
    }

    #[tokio::test]
    async fn creates_database_file_when_missing() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!("proto_rtc_storage_test_{suffix}"));
        let db_path = temp_root.join("nested").join("storage.db");
        let database_url = format!("sqlite://{}", db_path.to_string_lossy().replace('\\', "/"));

        let storage = Storage::new(&database_url).await.expect("db");
        drop(storage);

        assert!(
            db_path.exists(),
            "database file should exist: {}",
            db_path.display()
        );

        std::fs::remove_dir_all(temp_root).expect("cleanup");
    }

    #[tokio::test]
    async fn inserts_message_ciphertext() {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("bob").await.expect("user");
        let guild = storage.create_guild("ops", user).await.expect("guild");
        let channel = storage
            .create_channel(guild, "general", ChannelKind::Text)
            .await
            .expect("channel");
        let message = storage
            .insert_message_ciphertext(channel, user, b"opaque", &[])
            .await
            .expect("message");
        assert!(message.0 > 0);
    }

    #[tokio::test]
    async fn paginates_channel_messages() {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("bob").await.expect("user");
        let guild = storage.create_guild("ops", user).await.expect("guild");
        let channel = storage
            .create_channel(guild, "general", ChannelKind::Text)
            .await
            .expect("channel");

        let first = storage
            .insert_message_ciphertext(channel, user, b"first", &[])
            .await
            .expect("first");
        let second = storage
            .insert_message_ciphertext(channel, user, b"second", &[])
            .await
            .expect("second");
        let _third = storage
            .insert_message_ciphertext(channel, user, b"third", &[])
            .await
            .expect("third");

        let newest_two = storage
            .list_channel_messages(channel, 2, None)
            .await
            .expect("messages");
        assert_eq!(newest_two.len(), 2);
        assert_eq!(newest_two[0].message_id, second);

        let older = storage
            .list_channel_messages(channel, 2, Some(second.0))
            .await
            .expect("messages");
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].message_id, first);
    }

    #[tokio::test]
    async fn stores_message_attachments() {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("dave").await.expect("user");
        let guild = storage.create_guild("ops", user).await.expect("guild");
        let channel = storage
            .create_channel(guild, "general", ChannelKind::Text)
            .await
            .expect("channel");

        let attachments = vec![AttachmentMetadata {
            file_id: FileId(11),
            file_name: "report.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size_bytes: 1234,
        }];
        storage
            .insert_message_ciphertext(channel, user, b"hello", &attachments)
            .await
            .expect("message");

        let messages = storage
            .list_channel_messages(channel, 10, None)
            .await
            .expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].attachments, attachments);
    }

    #[tokio::test]
    async fn stores_latest_key_package_per_user_per_guild() {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("carol").await.expect("user");
        let guild = storage.create_guild("security", user).await.expect("guild");

        let first_id = storage
            .insert_key_package(guild, user, b"kp-1")
            .await
            .expect("insert key package");
        let second_id = storage
            .insert_key_package(guild, user, b"kp-2")
            .await
            .expect("insert key package");
        assert!(second_id > first_id);

        let latest = storage
            .load_latest_key_package(guild, user)
            .await
            .expect("latest")
            .expect("some latest");
        assert_eq!(latest.0, second_id);
        assert_eq!(latest.1, b"kp-2");
    }
}
