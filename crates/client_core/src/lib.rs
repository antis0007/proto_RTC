use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, GuildId},
    protocol::ClientRequest,
};

pub trait CryptoProvider: Send + Sync {
    fn encrypt_message(&self, plaintext: &[u8]) -> Vec<u8>;
    fn decrypt_message(&self, ciphertext: &[u8]) -> Vec<u8>;
}

/// TODO: replace with real E2EE provider implementation.
pub struct PassthroughCrypto;

impl CryptoProvider for PassthroughCrypto {
    fn encrypt_message(&self, plaintext: &[u8]) -> Vec<u8> {
        plaintext.to_vec()
    }

    fn decrypt_message(&self, ciphertext: &[u8]) -> Vec<u8> {
        ciphertext.to_vec()
    }
}

#[derive(Default, Debug, Clone)]
pub struct ClientState {
    pub user_id: Option<i64>,
    pub guilds: Vec<(i64, String)>,
    pub channels: Vec<(i64, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginRequest {
    username: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginResponse {
    user_id: i64,
}

pub struct CommunityClient<C: CryptoProvider> {
    http: Client,
    server_url: String,
    pub state: ClientState,
    crypto: C,
}

impl<C: CryptoProvider> CommunityClient<C> {
    pub fn new(server_url: impl Into<String>, crypto: C) -> Self {
        Self {
            http: Client::new(),
            server_url: server_url.into(),
            state: ClientState::default(),
            crypto,
        }
    }

    pub async fn login(&mut self, username: &str) -> Result<i64> {
        let res = self
            .http
            .post(format!("{}/login", self.server_url))
            .json(&LoginRequest {
                username: username.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        let body: LoginResponse = res.json().await?;
        self.state.user_id = Some(body.user_id);
        Ok(body.user_id)
    }

    pub fn send_message_request(&self, channel_id: ChannelId, plaintext: &str) -> ClientRequest {
        let ciphertext = self.crypto.encrypt_message(plaintext.as_bytes());
        ClientRequest::SendMessage {
            channel_id,
            ciphertext_b64: STANDARD.encode(ciphertext),
        }
    }

    pub fn request_livekit_token(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        can_publish_mic: bool,
        can_publish_screen: bool,
    ) -> ClientRequest {
        ClientRequest::RequestLiveKitToken {
            guild_id,
            channel_id,
            can_publish_mic,
            can_publish_screen,
        }
    }
}
