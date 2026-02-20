use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mls::{MlsStore, PersistedGroupSnapshot};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Pool, Row, Sqlite,
};
use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use shared::domain::{
    ChannelId, ChannelKind, DeviceId, DeviceLinkState, FileId, GuildId, LinkedDeviceSummary,
    MessageId, Role, UserId,
};

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
    pub attachment: Option<StoredAttachment>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoredAttachment {
    pub file_id: FileId,
    pub filename: String,
    pub size_bytes: u64,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredFile {
    pub file_id: FileId,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub ciphertext: Vec<u8>,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct StoredDeviceSummary {
    pub device_id: DeviceId,
    pub user_id: UserId,
    pub device_name: String,
    pub device_public_identity: String,
    pub is_revoked: bool,
}

#[derive(Debug, Clone)]
pub struct PendingWelcome {
    pub welcome_bytes: Vec<u8>,
    pub target_device_id: Option<DeviceId>,
}

#[derive(Debug, Clone)]
pub struct ConsumedPendingWelcome {
    pub welcome_bytes: Vec<u8>,
    pub consumed_at: DateTime<Utc>,
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

    pub async fn health_check(&self) -> Result<()> {
        let _: i64 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .context("sqlite ping failed")?;
        Ok(())
    }

    async fn ensure_pending_join_table(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS mls_pending_join_state (
                user_id           INTEGER NOT NULL,
                device_id         TEXT NOT NULL,
                guild_id          INTEGER NOT NULL,
                schema_version    INTEGER NOT NULL,
                group_state_blob  BLOB NOT NULL,
                key_material_blob BLOB NOT NULL,
                created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (user_id, device_id, guild_id)
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("failed to ensure mls_pending_join_state table exists")?;

        let pragma_rows = sqlx::query("PRAGMA table_info(mls_pending_join_state)")
            .fetch_all(&self.pool)
            .await
            .context("failed to inspect mls_pending_join_state schema")?;

        let mut has_created_at = false;
        let mut has_updated_at = false;
        for row in pragma_rows {
            let col_name: String = row.try_get("name")?;
            if col_name == "created_at" {
                has_created_at = true;
            } else if col_name == "updated_at" {
                has_updated_at = true;
            }
        }

        if !has_created_at {
            sqlx::query(
                "ALTER TABLE mls_pending_join_state ADD COLUMN created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
            )
            .execute(&self.pool)
            .await
            .context("failed adding created_at column to mls_pending_join_state")?;
        }

        if !has_updated_at {
            sqlx::query(
                "ALTER TABLE mls_pending_join_state ADD COLUMN updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
            )
            .execute(&self.pool)
            .await
            .context("failed adding updated_at column to mls_pending_join_state")?;
        }

        Ok(())
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
        attachment: Option<&StoredAttachment>,
    ) -> Result<MessageId> {
        let rec = sqlx::query(
            "INSERT INTO messages (channel_id, sender_user_id, ciphertext, attachment_file_id, attachment_filename, attachment_size_bytes, attachment_mime_type) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(channel_id.0)
        .bind(sender_id.0)
        .bind(ciphertext)
        .bind(attachment.map(|a| a.file_id.0))
        .bind(attachment.map(|a| a.filename.as_str()))
        .bind(attachment.map(|a| i64::try_from(a.size_bytes).unwrap_or(i64::MAX)))
        .bind(attachment.and_then(|a| a.mime_type.as_deref()))
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
                "SELECT id, channel_id, sender_user_id, ciphertext, created_at, attachment_file_id, attachment_filename, attachment_size_bytes, attachment_mime_type
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
                "SELECT id, channel_id, sender_user_id, ciphertext, created_at, attachment_file_id, attachment_filename, attachment_size_bytes, attachment_mime_type
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
            .map(|r| StoredMessage {
                message_id: MessageId(r.get::<i64, _>(0)),
                channel_id: ChannelId(r.get::<i64, _>(1)),
                sender_id: UserId(r.get::<i64, _>(2)),
                ciphertext: r.get::<Vec<u8>, _>(3),
                attachment: r.get::<Option<i64>, _>(5).map(|file_id| StoredAttachment {
                    file_id: FileId(file_id),
                    filename: r
                        .get::<Option<String>, _>(6)
                        .unwrap_or_else(|| "attachment.bin".to_string()),
                    size_bytes: r.get::<Option<i64>, _>(7).unwrap_or_default() as u64,
                    mime_type: r.get::<Option<String>, _>(8),
                }),
                created_at: r.get::<DateTime<Utc>, _>(4),
            })
            .collect())
    }

    pub async fn store_file_ciphertext(
        &self,
        uploader_id: UserId,
        guild_id: GuildId,
        channel_id: ChannelId,
        ciphertext: &[u8],
        mime: Option<&str>,
        filename: Option<&str>,
    ) -> Result<FileId> {
        let size_bytes = i64::try_from(ciphertext.len()).unwrap_or(i64::MAX);
        let rec = sqlx::query(
            "INSERT INTO files (uploader_user_id, guild_id, channel_id, ciphertext, mime_type, filename, size_bytes) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
         .bind(uploader_id.0)
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(ciphertext)
        .bind(mime)
        .bind(filename)
        .bind(size_bytes)
        .fetch_one(&self.pool)
        .await?;
        Ok(FileId(rec.get::<i64, _>(0)))
    }

    pub async fn load_file(&self, file_id: FileId) -> Result<Option<StoredFile>> {
        let row = sqlx::query(
            "SELECT id, guild_id, channel_id, ciphertext, mime_type, filename, size_bytes FROM files WHERE id = ?",
        )
            .bind(file_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| StoredFile {
            file_id: FileId(r.get::<i64, _>(0)),
            guild_id: GuildId(r.get::<i64, _>(1)),
            channel_id: ChannelId(r.get::<i64, _>(2)),
            ciphertext: r.get::<Vec<u8>, _>(3),
            mime_type: r.get::<Option<String>, _>(4),
            filename: r.get::<Option<String>, _>(5),
            size_bytes: r.get::<Option<i64>, _>(6).unwrap_or_default() as u64,
        }))
    }

    pub async fn register_device(
        &self,
        user_id: UserId,
        device_name: &str,
        device_public_identity: &str,
    ) -> Result<StoredDeviceSummary> {
        let row = sqlx::query(
            "INSERT INTO user_devices (user_id, device_name, device_public_identity)
             VALUES (?, ?, ?)
             RETURNING device_id, user_id, device_name, device_public_identity, is_revoked",
        )
        .bind(user_id.0)
        .bind(device_name)
        .bind(device_public_identity)
        .fetch_one(&self.pool)
        .await?;

        Ok(StoredDeviceSummary {
            device_id: DeviceId(row.get::<i64, _>(0)),
            user_id: UserId(row.get::<i64, _>(1)),
            device_name: row.get::<String, _>(2),
            device_public_identity: row.get::<String, _>(3),
            is_revoked: row.get::<bool, _>(4),
        })
    }

    pub async fn get_device(
        &self,
        user_id: UserId,
        device_id: DeviceId,
    ) -> Result<Option<StoredDeviceSummary>> {
        let row = sqlx::query(
            "SELECT device_id, user_id, device_name, device_public_identity, is_revoked
             FROM user_devices
             WHERE user_id = ? AND device_id = ?",
        )
        .bind(user_id.0)
        .bind(device_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| StoredDeviceSummary {
            device_id: DeviceId(row.get::<i64, _>(0)),
            user_id: UserId(row.get::<i64, _>(1)),
            device_name: row.get::<String, _>(2),
            device_public_identity: row.get::<String, _>(3),
            is_revoked: row.get::<bool, _>(4),
        }))
    }

    pub async fn list_devices_for_user(&self, user_id: UserId) -> Result<Vec<LinkedDeviceSummary>> {
        let rows = sqlx::query(
            "SELECT device_id, user_id, device_name, device_public_identity, is_revoked
             FROM user_devices
             WHERE user_id = ?
             ORDER BY device_id ASC",
        )
        .bind(user_id.0)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let is_revoked = row.get::<bool, _>(4);
                LinkedDeviceSummary {
                    device: shared::domain::DeviceSummary {
                        device_id: DeviceId(row.get::<i64, _>(0)),
                        user_id: UserId(row.get::<i64, _>(1)),
                        device_name: row.get::<String, _>(2),
                        device_public_identity: row.get::<String, _>(3),
                        is_revoked,
                    },
                    link_state: if is_revoked {
                        DeviceLinkState::Revoked
                    } else {
                        DeviceLinkState::Linked
                    },
                }
            })
            .collect())
    }

    pub async fn create_device_link_token(
        &self,
        user_id: UserId,
        initiator_device_id: DeviceId,
        target_device_pubkey: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<i64> {
        let row = sqlx::query(
            "INSERT INTO device_link_tokens (user_id, initiator_device_id, target_device_pubkey, expires_at)
             VALUES (?, ?, ?, ?)
             RETURNING token_id",
        )
        .bind(user_id.0)
        .bind(initiator_device_id.0)
        .bind(target_device_pubkey)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    pub async fn consume_device_link_token(&self, user_id: UserId, token_id: i64) -> Result<bool> {
        let updated = sqlx::query(
            "UPDATE device_link_tokens
             SET consumed_at = CURRENT_TIMESTAMP
             WHERE token_id = ? AND user_id = ? AND consumed_at IS NULL AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(token_id)
        .bind(user_id.0)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated > 0)
    }

    pub async fn insert_key_package(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        device_id: Option<DeviceId>,
        key_package_bytes: &[u8],
    ) -> Result<i64> {
        let rec = sqlx::query(
            "INSERT INTO mls_key_packages (guild_id, user_id, device_id, key_package_bytes)
             VALUES (?, ?, ?, ?)
             RETURNING id",
        )
        .bind(guild_id.0)
        .bind(user_id.0)
        .bind(device_id.map(|id| id.0))
        .bind(key_package_bytes)
        .fetch_one(&self.pool)
        .await?;
        Ok(rec.get::<i64, _>(0))
    }

    pub async fn load_latest_key_package(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        target_device_id: Option<DeviceId>,
    ) -> Result<Option<(i64, Option<DeviceId>, Vec<u8>)>> {
        let row = if let Some(device_id) = target_device_id {
            sqlx::query(
                "SELECT id, device_id, key_package_bytes
                 FROM mls_key_packages
                 WHERE guild_id = ? AND user_id = ? AND device_id = ?
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(user_id.0)
            .bind(device_id.0)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT kp.id, kp.device_id, kp.key_package_bytes
                 FROM mls_key_packages kp
                 LEFT JOIN user_devices ud ON ud.device_id = kp.device_id
                 WHERE kp.guild_id = ? AND kp.user_id = ? AND (kp.device_id IS NULL OR COALESCE(ud.is_revoked, 0) = 0)
                 ORDER BY kp.id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(user_id.0)
            .fetch_optional(&self.pool)
            .await?
        };

        Ok(row.map(|r| {
            (
                r.get::<i64, _>(0),
                r.get::<Option<i64>, _>(1).map(DeviceId),
                r.get::<Vec<u8>, _>(2),
            )
        }))
    }

    pub async fn insert_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        user_id: UserId,
        target_device_id: Option<DeviceId>,
        welcome_bytes: &[u8],
    ) -> Result<i64> {
        let rec = sqlx::query(
            "INSERT INTO pending_welcomes (guild_id, channel_id, user_id, target_device_id, welcome_bytes)
             VALUES (?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(user_id.0)
        .bind(target_device_id.map(|id| id.0))
        .bind(welcome_bytes)
        .fetch_one(&self.pool)
        .await?;

        Ok(rec.get::<i64, _>(0))
    }

    pub async fn load_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        user_id: UserId,
        target_device_id: Option<DeviceId>,
    ) -> Result<Option<PendingWelcome>> {
        let row = if let Some(device_id) = target_device_id {
            sqlx::query(
                "SELECT welcome_bytes, target_device_id
                 FROM pending_welcomes
                 WHERE guild_id = ? AND channel_id = ? AND user_id = ? AND target_device_id = ? AND consumed_at IS NULL
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(channel_id.0)
            .bind(user_id.0)
            .bind(device_id.0)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT welcome_bytes, target_device_id
                 FROM pending_welcomes
                 WHERE guild_id = ? AND channel_id = ? AND user_id = ? AND consumed_at IS NULL
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(channel_id.0)
            .bind(user_id.0)
            .fetch_optional(&self.pool)
            .await?
        };

        Ok(row.map(|r| PendingWelcome {
            welcome_bytes: r.get::<Vec<u8>, _>(0),
            target_device_id: r.get::<Option<i64>, _>(1).map(DeviceId),
        }))
    }

    pub async fn load_latest_welcome_any_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        user_id: UserId,
        target_device_id: Option<DeviceId>,
    ) -> Result<Option<PendingWelcome>> {
        let row = if let Some(device_id) = target_device_id {
            sqlx::query(
                "SELECT welcome_bytes, target_device_id
                 FROM pending_welcomes
                 WHERE guild_id = ? AND channel_id = ? AND user_id = ? AND target_device_id = ?
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(channel_id.0)
            .bind(user_id.0)
            .bind(device_id.0)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT welcome_bytes, target_device_id
                 FROM pending_welcomes
                 WHERE guild_id = ? AND channel_id = ? AND user_id = ?
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .bind(guild_id.0)
            .bind(channel_id.0)
            .bind(user_id.0)
            .fetch_optional(&self.pool)
            .await?
        };

        Ok(row.map(|r| PendingWelcome {
            welcome_bytes: r.get::<Vec<u8>, _>(0),
            target_device_id: r.get::<Option<i64>, _>(1).map(DeviceId),
        }))
    }

    pub async fn load_and_consume_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        user_id: UserId,
    ) -> Result<Option<ConsumedPendingWelcome>> {
        let row = sqlx::query(
            "UPDATE pending_welcomes
             SET consumed_at = CURRENT_TIMESTAMP
             WHERE id = (
                SELECT id
                FROM pending_welcomes
                WHERE guild_id = ? AND channel_id = ? AND user_id = ? AND consumed_at IS NULL
                ORDER BY id DESC
                LIMIT 1
             )
             RETURNING welcome_bytes, consumed_at",
        )
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(user_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ConsumedPendingWelcome {
            welcome_bytes: r.get::<Vec<u8>, _>(0),
            consumed_at: r.get::<DateTime<Utc>, _>(1),
        }))
    }

    /// Deletes the persisted MLS group snapshot for a single channel on this device.
    ///
    /// This is the main self-heal primitive for stale local MLS roster state (e.g. the
    /// client believes a member is in the roster, but the server has no welcome history
    /// after a dev/test DB reset). Identity keys are intentionally preserved.
    pub async fn clear_mls_group_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM mls_group_states WHERE user_id = ? AND device_id = ? AND guild_id = ? AND channel_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .bind(channel_id.0)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Deletes all persisted MLS group snapshots for a user/device pair while keeping
    /// identity keys intact. Useful when the server database has been reset in local dev.
    pub async fn clear_all_mls_group_states_for_device(
        &self,
        user_id: i64,
        device_id: &str,
    ) -> Result<u64> {
        let result =
            sqlx::query("DELETE FROM mls_group_states WHERE user_id = ? AND device_id = ?")
                .bind(user_id)
                .bind(device_id)
                .execute(&self.pool)
                .await?;

        Ok(result.rows_affected())
    }

    /// Full local MLS reset for a user/device: clears group snapshots and identity keys.
    /// Prefer `clear_mls_group_state` or `clear_all_mls_group_states_for_device` first;
    /// this is a stronger fallback if local cryptographic material is corrupted.
    pub async fn clear_all_mls_state_for_device(
        &self,
        user_id: i64,
        device_id: &str,
    ) -> Result<(u64, u64)> {
        let mut tx = self.pool.begin().await?;

        let groups =
            sqlx::query("DELETE FROM mls_group_states WHERE user_id = ? AND device_id = ?")
                .bind(user_id)
                .bind(device_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();

        let identities =
            sqlx::query("DELETE FROM mls_identity_keys WHERE user_id = ? AND device_id = ?")
                .bind(user_id)
                .bind(device_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();

        tx.commit().await?;
        Ok((groups, identities))
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
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
        snapshot: PersistedGroupSnapshot,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO mls_group_states (
                user_id,
                device_id,
                guild_id,
                channel_id,
                schema_version,
                group_state_blob,
                key_material_blob,
                group_state_bytes,
                updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(user_id, device_id, guild_id, channel_id) DO UPDATE SET
                schema_version = excluded.schema_version,
                group_state_blob = excluded.group_state_blob,
                key_material_blob = excluded.key_material_blob,
                group_state_bytes = excluded.group_state_bytes,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .bind(channel_id.0)
        .bind(snapshot.schema_version)
        .bind(snapshot.group_state_blob)
        .bind(snapshot.key_material_blob)
        .bind(Vec::<u8>::new())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_group_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<PersistedGroupSnapshot>> {
        let row = sqlx::query(
            "SELECT schema_version, group_state_blob, key_material_blob, group_state_bytes
             FROM mls_group_states
             WHERE user_id = ? AND device_id = ? AND guild_id = ? AND channel_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .bind(channel_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let schema_version = r.get::<i64, _>(0) as i32;
            let group_state_blob = r.get::<Option<Vec<u8>>, _>(1);
            let key_material_blob = r.get::<Option<Vec<u8>>, _>(2);
            let legacy_group_state = r.get::<Vec<u8>, _>(3);

            match (group_state_blob, key_material_blob) {
                (Some(group_state_blob), Some(key_material_blob)) => PersistedGroupSnapshot {
                    schema_version,
                    group_state_blob,
                    key_material_blob,
                },
                _ => PersistedGroupSnapshot {
                    schema_version: 0,
                    group_state_blob: legacy_group_state,
                    key_material_blob: Vec::new(),
                },
            }
        }))
    }

    async fn save_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        snapshot: PersistedGroupSnapshot,
    ) -> Result<()> {
        self.ensure_pending_join_table().await?;
        sqlx::query(
            r#"
            INSERT INTO mls_pending_join_state (
                user_id,
                device_id,
                guild_id,
                schema_version,
                group_state_blob,
                key_material_blob,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(user_id, device_id, guild_id) DO UPDATE SET
                schema_version = excluded.schema_version,
                group_state_blob = excluded.group_state_blob,
                key_material_blob = excluded.key_material_blob,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .bind(snapshot.schema_version)
        .bind(snapshot.group_state_blob)
        .bind(snapshot.key_material_blob)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
    ) -> Result<Option<PersistedGroupSnapshot>> {
        self.ensure_pending_join_table().await?;
        let row = sqlx::query(
            r#"
            SELECT schema_version, group_state_blob, key_material_blob
            FROM mls_pending_join_state
            WHERE user_id = ?1 AND device_id = ?2 AND guild_id = ?3
            "#,
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| PersistedGroupSnapshot {
            schema_version: row.get::<i32, _>("schema_version"),
            group_state_blob: row.get::<Vec<u8>, _>("group_state_blob"),
            key_material_blob: row.get::<Vec<u8>, _>("key_material_blob"),
        }))
    }

    async fn clear_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
    ) -> Result<()> {
        self.ensure_pending_join_table().await?;
        sqlx::query(
            "DELETE FROM mls_pending_join_state WHERE user_id = ?1 AND device_id = ?2 AND guild_id = ?3",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(guild_id.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/lib_tests.rs"]
mod tests;
