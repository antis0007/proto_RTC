use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId, Role, UserId},
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_username: Option<String>,
    pub ciphertext_b64: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentMetadata>,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentMetadata {
    pub file_id: FileId,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: i64,
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
    FileStored {
        file_id: FileId,
    },
    Error(ApiError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChannelId, FileId, MessageId, UserId};

    #[test]
    fn message_payload_omits_empty_attachments() {
        let payload = MessagePayload {
            message_id: MessageId(1),
            channel_id: ChannelId(2),
            sender_id: UserId(3),
            sender_username: Some("alice".to_string()),
            ciphertext_b64: "dGV4dA==".to_string(),
            attachments: Vec::new(),
            sent_at: Utc::now(),
        };

        let json = serde_json::to_value(payload).expect("serialize payload");
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn message_payload_roundtrips_with_attachments() {
        let payload = MessagePayload {
            message_id: MessageId(1),
            channel_id: ChannelId(2),
            sender_id: UserId(3),
            sender_username: Some("alice".to_string()),
            ciphertext_b64: "dGV4dA==".to_string(),
            attachments: vec![AttachmentMetadata {
                file_id: FileId(9),
                file_name: "hello.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size_bytes: 5,
            }],
            sent_at: Utc::now(),
        };

        let json = serde_json::to_vec(&payload).expect("serialize payload");
        let decoded: MessagePayload = serde_json::from_slice(&json).expect("deserialize payload");
        assert_eq!(decoded.attachments.len(), 1);
        assert_eq!(decoded.attachments[0].file_name, "hello.txt");
    }
}
