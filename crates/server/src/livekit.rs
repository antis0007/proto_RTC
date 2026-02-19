use chrono::{Duration, Utc};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use shared::domain::{ChannelId, GuildId, UserId};

#[derive(Debug, Clone)]
pub struct LiveKitConfig {
    pub api_key: String,
    pub api_secret: String,
    pub ttl_seconds: i64,
}

#[derive(Debug, Serialize)]
struct VideoGrant {
    room_join: bool,
    room: String,
    can_publish: bool,
    can_subscribe: bool,
    can_publish_data: bool,
}

#[derive(Debug, Serialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: i64,
    iat: i64,
    video: VideoGrant,
    metadata: String,
}

pub fn room_name_for_voice_channel(guild_id: GuildId, channel_id: ChannelId) -> String {
    format!("g:{}:c:{}", guild_id.0, channel_id.0)
}

pub fn mint_token(
    cfg: &LiveKitConfig,
    user_id: UserId,
    room_name: &str,
    can_publish_mic: bool,
    can_publish_screen: bool,
) -> Result<String, jsonwebtoken::errors::Error> {
    let can_publish = can_publish_mic || can_publish_screen;
    let now = Utc::now();
    let exp = now + Duration::seconds(cfg.ttl_seconds);
    let claims = Claims {
        iss: cfg.api_key.clone(),
        sub: format!("user:{}", user_id.0),
        iat: now.timestamp(),
        exp: exp.timestamp(),
        video: VideoGrant {
            room_join: true,
            room: room_name.to_string(),
            can_publish,
            can_subscribe: true,
            can_publish_data: true,
        },
        metadata: format!(
            "{{\"can_publish_mic\":{},\"can_publish_screen\":{}}}",
            can_publish_mic, can_publish_screen
        ),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(cfg.api_secret.as_bytes()),
    )
}

#[cfg(test)]
#[path = "tests/livekit_tests.rs"]
mod tests;
