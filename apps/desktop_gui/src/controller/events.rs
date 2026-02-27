//! UI/backend events and error modeling for desktop GUI controller.

use client_core::{VoiceParticipantState, VoiceSessionSnapshot};
use shared::{
    domain::{ChannelId, FileId, GuildId},
    protocol::{MessagePayload, ServerEvent},
};

use crate::ui::app::{DecodedGifPreview, PreviewImage};

pub enum UiEvent {
    LoginOk,
    Info(String),
    InviteCreated(String),
    JoinedGuild(GuildId),
    SenderDirectoryUpdated {
        user_id: i64,
        username: String,
    },
    Error(UiError),
    AttachmentPreviewLoaded {
        file_id: FileId,
        image: PreviewImage,
        original_bytes: Vec<u8>,
        decoded_gif: Option<DecodedGifPreview>,
    },
    AttachmentPreviewFailed {
        file_id: FileId,
        reason: String,
    },
    Server(ServerEvent),
    MessageDecrypted {
        message: MessagePayload,
        plaintext: String,
    },
    VoiceSessionStateChanged(Option<VoiceSessionSnapshot>),
    VoiceParticipantsUpdated {
        guild_id: GuildId,
        channel_id: ChannelId,
        participants: Vec<VoiceParticipantState>,
    },
    VoiceOperationFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiErrorCategory {
    Auth,
    Transport,
    Crypto,
    Validation,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiErrorContext {
    BackendStartup,
    Login,
    SendMessage,
    DecryptMessage,
    General,
}

pub fn classify_login_failure(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("backend worker startup failure")
        || lower.contains("failed to initialize persistent mls backend")
        || lower.contains("failed to build backend runtime")
    {
        "Backend worker startup failure; verify local app environment and retry.".to_string()
    } else if lower.contains("failed to connect")
        || lower.contains("connection refused")
        || lower.contains("dns")
        || lower.contains("timed out")
    {
        "Server unreachable; check URL/network and retry sign-in.".to_string()
    } else {
        format!("Login/API error: {message}")
    }
}

#[derive(Debug, Clone)]
pub struct UiError {
    category: UiErrorCategory,
    context: UiErrorContext,
    message: String,
}

impl UiError {
    pub fn from_message(context: UiErrorContext, message: impl Into<String>) -> Self {
        let message = message.into();
        let message_lower = message.to_ascii_lowercase();
        let category = if message_lower.contains("401")
            || message_lower.contains("403")
            || message_lower.contains("unauthorized")
            || message_lower.contains("forbidden")
            || message_lower.contains("session expired")
            || message_lower.contains("invalid token")
            || message_lower.contains("invalid credential")
        {
            UiErrorCategory::Auth
        } else if message_lower.contains("decrypt")
            || message_lower.contains("encrypt")
            || message_lower.contains("mls")
            || message_lower.contains("cipher")
            || message_lower.contains("crypto")
        {
            UiErrorCategory::Crypto
        } else if message_lower.contains("invalid")
            || message_lower.contains("missing")
            || message_lower.contains("malformed")
        {
            UiErrorCategory::Validation
        } else if message_lower.contains("timeout")
            || message_lower.contains("connection")
            || message_lower.contains("network")
            || message_lower.contains("transport")
            || message_lower.contains("unavailable")
            || message_lower.contains("disconnect")
        {
            UiErrorCategory::Transport
        } else {
            UiErrorCategory::Unknown
        };

        Self {
            category,
            context,
            message,
        }
    }

    pub fn requires_reauth(&self) -> bool {
        self.category == UiErrorCategory::Auth
    }

    pub fn category(&self) -> UiErrorCategory {
        self.category
    }

    pub fn context(&self) -> UiErrorContext {
        self.context
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}
