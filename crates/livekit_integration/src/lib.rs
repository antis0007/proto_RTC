use async_trait::async_trait;
use chrono::{Duration, Utc};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use shared::domain::{ChannelId, GuildId, UserId};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveKitRoomOptions {
    pub room_name: String,
    pub token: String,
    pub e2ee_key: Vec<u8>,
    pub e2ee_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTrack {
    Microphone,
    ScreenShare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParticipant {
    pub participant_id: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveKitRoomEvent {
    ParticipantJoined(RemoteParticipant),
    ParticipantLeft { participant_id: String },
}

#[async_trait]
pub trait LiveKitRoomSession: Send + Sync {
    async fn publish_local_track(&self, track: LocalTrack) -> anyhow::Result<()>;
    async fn unpublish_local_tracks(&self) -> anyhow::Result<()>;
    async fn leave(&self) -> anyhow::Result<()>;
    fn supports_screen_share(&self) -> bool;
    fn is_e2ee_enabled(&self) -> bool;
    fn subscribe_events(&self) -> broadcast::Receiver<LiveKitRoomEvent>;
}

#[async_trait]
pub trait LiveKitRoomConnector: Send + Sync {
    async fn connect(
        &self,
        options: LiveKitRoomOptions,
    ) -> anyhow::Result<std::sync::Arc<dyn LiveKitRoomSession>>;
}

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
mod tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};

    #[test]
    fn deterministic_room_name() {
        let room = room_name_for_voice_channel(GuildId(10), ChannelId(42));
        assert_eq!(room, "g:10:c:42");
    }

    #[test]
    fn token_claims_contain_room_and_subject() {
        let cfg = LiveKitConfig {
            api_key: "devkey".into(),
            api_secret: "devsecret".into(),
            ttl_seconds: 60,
        };
        let token = mint_token(&cfg, UserId(7), "g:1:c:2", true, false).expect("token");

        let decoded = decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret(cfg.api_secret.as_bytes()),
            &Validation::default(),
        )
        .expect("decode");

        assert_eq!(decoded.claims["iss"], "devkey");
        assert_eq!(decoded.claims["sub"], "user:7");
        assert_eq!(decoded.claims["video"]["room"], "g:1:c:2");
    }
}
