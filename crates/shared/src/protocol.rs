use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{ChannelId, ChannelKind, DeviceId, FileId, GuildId, MessageId, Role, UserId},
    error::ApiError,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MlsBootstrapReason {
    #[default]
    Unknown,
    MissingPendingWelcome,
    LocalStateMissing,
    RecoveryWelcomeDuplicateMember,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientRequest {
    Login {
        username: String,
    },
    JoinGuild {
        guild_id: GuildId,
    },
    ListGuilds,
    ListChannels {
        guild_id: GuildId,
    },
    SendMessage {
        channel_id: ChannelId,
        ciphertext_b64: String,
    },
    CreateInvite {
        guild_id: GuildId,
    },
    Kick {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    Ban {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    Mute {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    RequestLiveKitToken {
        guild_id: GuildId,
        channel_id: ChannelId,
        can_publish_mic: bool,
        can_publish_screen: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildSummary {
    pub guild_id: GuildId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub channel_id: ChannelId,
    pub guild_id: GuildId,
    pub kind: ChannelKind,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadKeyPackageResponse {
    pub key_package_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyPackageResponse {
    pub key_package_id: i64,
    pub guild_id: i64,
    pub user_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub key_package_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeResponse {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<DeviceId>,
    pub welcome_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelStateRecord {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub mls_group_state_blob_b64: String,
    pub checkpoint_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id_seen: Option<MessageId>,
    pub state_hash_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedChannelStateBundleV1 {
    pub version: u8,
    pub source_device_id: DeviceId,
    pub target_device_id: DeviceId,
    pub created_at: DateTime<Utc>,
    pub channels: Vec<ChannelStateRecord>,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
    pub aad_b64: String,
    pub signature_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLinkStartResponse {
    pub token_id: i64,
    pub token_secret: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLinkBundleUploadRequest {
    pub token_id: i64,
    pub token_secret: String,
    pub source_device_id: DeviceId,
    pub bundle: EncryptedChannelStateBundleV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLinkBundleFetchRequest {
    pub token_id: i64,
    pub token_secret: String,
    pub target_device_id: DeviceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePayload {
    pub message_id: MessageId,
    pub channel_id: ChannelId,
    pub sender_id: UserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_username: Option<String>,
    pub ciphertext_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentPayload>,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPayload {
    pub file_id: FileId,
    pub filename: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberSummary {
    pub guild_id: GuildId,
    pub user_id: UserId,
    pub username: String,
    pub role: Role,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ServerEvent {
    GuildUpdated {
        guild: GuildSummary,
    },
    ChannelUpdated {
        channel: ChannelSummary,
    },
    GuildMembersUpdated {
        guild_id: GuildId,
        members: Vec<MemberSummary>,
    },
    MessageReceived {
        message: MessagePayload,
    },
    UserKicked {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    UserBanned {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    UserMuted {
        guild_id: GuildId,
        target_user_id: UserId,
    },
    LiveKitTokenIssued {
        guild_id: GuildId,
        channel_id: ChannelId,
        room_name: String,
        token: String,
    },
    MlsWelcomeAvailable {
        guild_id: GuildId,
        channel_id: ChannelId,
        target_user_id: UserId,
        #[serde(default)]
        target_device_id: Option<DeviceId>,
    },
    MlsBootstrapRequested {
        guild_id: GuildId,
        channel_id: ChannelId,
        requesting_user_id: UserId,
        #[serde(default)]
        target_user_id: Option<UserId>,
        #[serde(default)]
        target_device_id: Option<DeviceId>,
        #[serde(default)]
        reason: MlsBootstrapReason,
    },
    FileStored {
        file_id: FileId,
    },
    Error(ApiError),
}
