//! Backend commands queued from UI to backend worker.

use shared::domain::{ChannelId, FileId, GuildId, MessageId};
use std::path::PathBuf;

pub enum BackendCommand {
    Login {
        server_url: String,
        username: String,
    },
    ListGuilds,
    ListChannels {
        guild_id: GuildId,
    },
    ListMembers {
        guild_id: GuildId,
    },
    SelectChannel {
        channel_id: ChannelId,
    },
    LoadMoreMessages {
        channel_id: ChannelId,
        before: MessageId,
    },
    SendMessage {
        text: String,
        attachment_path: Option<PathBuf>,
    },
    DownloadAttachment {
        file_id: FileId,
        filename: String,
    },
    FetchAttachmentPreview {
        file_id: FileId,
    },
    CreateInvite {
        guild_id: GuildId,
    },
    JoinWithInvite {
        invite_code: String,
    },
    ConnectVoice {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    DisconnectVoice,
}
