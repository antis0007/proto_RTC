use crate::livekit::{mint_token, room_name_for_voice_channel, LiveKitConfig};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, ChannelKind, GuildId, Role, UserId},
    error::{ApiError, ErrorCode},
    protocol::{
        AttachmentPayload, ChannelSummary, GuildSummary, MemberSummary, MessagePayload, ServerEvent,
    },
};
use storage::{Storage, StoredAttachment};

#[derive(Clone)]
pub struct ApiContext {
    pub storage: Storage,
    pub livekit: LiveKitConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MlsKeyPackageQuery {
    pub guild_id: i64,
    pub user_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MlsWelcomeQuery {
    pub user_id: i64,
    pub guild_id: i64,
    pub channel_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UploadKeyPackageResponse {
    pub key_package_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct KeyPackageResponse {
    pub key_package_id: i64,
    pub guild_id: i64,
    pub user_id: i64,
    pub key_package_b64: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MlsWelcomeResponse {
    pub user_id: i64,
    pub guild_id: i64,
    pub channel_id: i64,
    pub welcome_b64: String,
    pub consumed_at: Option<DateTime<Utc>>,
}

pub fn mls_key_packages_route() -> &'static str {
    "/mls/key_packages"
}

pub fn mls_welcome_route() -> &'static str {
    "/mls/welcome"
}

pub fn mls_bootstrap_request_route() -> &'static str {
    "/mls/bootstrap/request"
}

pub fn mls_welcome_recovery_route() -> &'static str {
    "/mls/welcome/recovery"
}

pub async fn list_guilds(ctx: &ApiContext, user_id: UserId) -> Result<Vec<GuildSummary>, ApiError> {
    let guilds = ctx
        .storage
        .list_guilds_for_user(user_id)
        .await
        .map_err(internal)?;
    Ok(guilds
        .into_iter()
        .map(|(guild_id, name)| GuildSummary { guild_id, name })
        .collect())
}

pub async fn list_channels(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
) -> Result<Vec<ChannelSummary>, ApiError> {
    ensure_active_membership(ctx, guild_id, user_id).await?;
    let channels = ctx
        .storage
        .list_channels_for_guild(guild_id)
        .await
        .map_err(internal)?;
    Ok(channels
        .into_iter()
        .map(|(channel_id, name, kind)| ChannelSummary {
            channel_id,
            guild_id,
            kind,
            name,
        })
        .collect())
}

pub async fn list_members(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
) -> Result<Vec<MemberSummary>, ApiError> {
    ensure_active_membership(ctx, guild_id, user_id).await?;
    let members = ctx
        .storage
        .list_members_for_guild(guild_id)
        .await
        .map_err(internal)?;

    Ok(members
        .into_iter()
        .map(|member| MemberSummary {
            guild_id,
            user_id: member.user_id,
            username: member.username,
            role: member.role,
            muted: member.muted,
        })
        .collect())
}

pub async fn send_message(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
    channel_id: ChannelId,
    ciphertext_b64: &str,
    attachment: Option<AttachmentPayload>,
) -> Result<ServerEvent, ApiError> {
    let (_, _, muted) = ensure_active_membership(ctx, guild_id, user_id).await?;
    let actual_guild_id = ctx
        .storage
        .guild_for_channel(channel_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    if actual_guild_id != guild_id {
        return Err(ApiError::new(
            ErrorCode::Validation,
            "channel does not belong to guild",
        ));
    }
    if muted {
        return Err(ApiError::new(ErrorCode::Forbidden, "user is muted"));
    }
    let ciphertext = STANDARD
        .decode(ciphertext_b64)
        .map_err(|_| ApiError::new(ErrorCode::Validation, "invalid base64 ciphertext"))?;

    let stored_attachment = attachment.as_ref().map(|attachment| StoredAttachment {
        file_id: attachment.file_id,
        filename: attachment.filename.clone(),
        size_bytes: attachment.size_bytes,
        mime_type: attachment.mime_type.clone(),
    });

    let message_id = ctx
        .storage
        .insert_message_ciphertext(channel_id, user_id, &ciphertext, stored_attachment.as_ref())
        .await
        .map_err(internal)?;
    let sender_username = ctx
        .storage
        .username_for_user(user_id)
        .await
        .map_err(internal)?;
    Ok(ServerEvent::MessageReceived {
        message: MessagePayload {
            message_id,
            channel_id,
            sender_id: user_id,
            sender_username,
            ciphertext_b64: ciphertext_b64.to_string(),
            attachment,
            sent_at: Utc::now(),
        },
    })
}

pub async fn list_messages(
    ctx: &ApiContext,
    user_id: UserId,
    channel_id: ChannelId,
    limit: u32,
    before: Option<i64>,
) -> Result<Vec<MessagePayload>, ApiError> {
    let guild_id = ctx
        .storage
        .guild_for_channel(channel_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    ensure_active_membership(ctx, guild_id, user_id).await?;

    let messages = ctx
        .storage
        .list_channel_messages(channel_id, limit, before)
        .await
        .map_err(internal)?;

    let mut username_cache: std::collections::HashMap<UserId, Option<String>> =
        std::collections::HashMap::new();
    let mut payloads = Vec::with_capacity(messages.len());
    for message in messages {
        let sender_username = if let Some(cached) = username_cache.get(&message.sender_id) {
            cached.clone()
        } else {
            let resolved = ctx
                .storage
                .username_for_user(message.sender_id)
                .await
                .map_err(internal)?;
            username_cache.insert(message.sender_id, resolved.clone());
            resolved
        };

        payloads.push(MessagePayload {
            message_id: message.message_id,
            channel_id: message.channel_id,
            sender_id: message.sender_id,
            sender_username,
            ciphertext_b64: STANDARD.encode(message.ciphertext),
            attachment: message.attachment.map(|attachment| AttachmentPayload {
                file_id: attachment.file_id,
                filename: attachment.filename,
                size_bytes: attachment.size_bytes,
                mime_type: attachment.mime_type,
            }),
            sent_at: message.created_at,
        });
    }

    Ok(payloads)
}

pub async fn request_livekit_token(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
    channel_id: ChannelId,
    can_publish_mic: bool,
    can_publish_screen: bool,
) -> Result<ServerEvent, ApiError> {
    ensure_active_membership(ctx, guild_id, user_id).await?;
    let channels = ctx
        .storage
        .list_channels_for_guild(guild_id)
        .await
        .map_err(internal)?;
    let is_voice = channels
        .into_iter()
        .find(|(id, _, _)| *id == channel_id)
        .map(|(_, _, kind)| kind == ChannelKind::Voice)
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    if !is_voice {
        return Err(ApiError::new(
            ErrorCode::Validation,
            "livekit token only valid for voice channels",
        ));
    }

    let room_name = room_name_for_voice_channel(guild_id, channel_id);
    let token = mint_token(
        &ctx.livekit,
        user_id,
        &room_name,
        can_publish_mic,
        can_publish_screen,
    )
    .map_err(|e| ApiError::new(ErrorCode::Internal, format!("token mint failed: {e}")))?;

    Ok(ServerEvent::LiveKitTokenIssued {
        guild_id,
        channel_id,
        room_name,
        token,
    })
}

pub async fn ensure_active_membership_in_channel(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Result<(), ApiError> {
    ensure_active_membership(ctx, guild_id, user_id).await?;
    let actual_guild_id = ctx
        .storage
        .guild_for_channel(channel_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    if actual_guild_id != guild_id {
        return Err(ApiError::new(
            ErrorCode::Validation,
            "channel does not belong to guild",
        ));
    }
    Ok(())
}

pub async fn ensure_active_membership_in_guild(
    ctx: &ApiContext,
    user_id: UserId,
    guild_id: GuildId,
) -> Result<(), ApiError> {
    ensure_active_membership(ctx, guild_id, user_id).await?;
    Ok(())
}

async fn ensure_active_membership(
    ctx: &ApiContext,
    guild_id: GuildId,
    user_id: UserId,
) -> Result<(Role, bool, bool), ApiError> {
    let membership = ctx
        .storage
        .membership_status(guild_id, user_id)
        .await
        .map_err(internal)?;
    let Some((role, banned, muted)) = membership else {
        return Err(ApiError::new(ErrorCode::Forbidden, "user is not a member"));
    };
    if banned {
        return Err(ApiError::new(ErrorCode::Forbidden, "user is banned"));
    }
    Ok((role, banned, muted))
}

fn internal(err: anyhow::Error) -> ApiError {
    ApiError::new(ErrorCode::Internal, err.to_string())
}

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
