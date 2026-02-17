use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId, UserId},
    error::ApiError,
};

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
pub struct MessagePayload {
    pub message_id: MessageId,
    pub channel_id: ChannelId,
    pub sender_id: UserId,
    pub ciphertext_b64: String,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageContent {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachment {
    pub file_id: FileId,
    pub file_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
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
    FileStored {
        file_id: FileId,
    },
    Error(ApiError),
}
