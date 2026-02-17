use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures::StreamExt;
use mls::MlsIdentity;
use openmls_rust_crypto::OpenMlsRustCrypto;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, FileId, GuildId, MessageId},
    protocol::{
        AttachmentPayload, ChannelSummary, ClientRequest, GuildSummary, KeyPackageResponse,
        MemberSummary, MessagePayload, ServerEvent, UploadKeyPackageResponse,
    },
};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[async_trait]
pub trait MlsSessionManager: Send + Sync {
    async fn encrypt_application(&self, channel_id: ChannelId, plaintext: &[u8])
        -> Result<Vec<u8>>;
    async fn decrypt_application(
        &self,
        channel_id: ChannelId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>>;
}

pub struct MissingMlsSessionManager;

#[async_trait]
impl MlsSessionManager for MissingMlsSessionManager {
    async fn encrypt_application(
        &self,
        channel_id: ChannelId,
        _plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }

    async fn decrypt_application(
        &self,
        channel_id: ChannelId,
        _ciphertext: &[u8],
    ) -> Result<Vec<u8>> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }
}

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

#[derive(Debug, Serialize)]
struct JoinGuildRequest {
    user_id: i64,
    invite_code: String,
}

#[derive(Debug, Deserialize)]
struct InviteResponse {
    invite_code: String,
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

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Server(ServerEvent),
    MessageDecrypted {
        message: MessagePayload,
        plaintext: String,
    },
    UserDirectoryUpdated {
        user_id: i64,
        username: String,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct AttachmentUpload {
    pub filename: String,
    pub mime_type: Option<String>,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct FileUploadResponse {
    file_id: i64,
    size_bytes: usize,
}

#[async_trait]
pub trait ClientHandle: Send + Sync {
    async fn login(&self, server_url: &str, username: &str, password_or_invite: &str)
        -> Result<()>;
    async fn list_guilds(&self) -> Result<()>;
    async fn list_channels(&self, guild_id: GuildId) -> Result<()>;
    async fn list_members(&self, guild_id: GuildId) -> Result<Vec<MemberSummary>>;
    async fn select_channel(&self, channel_id: ChannelId) -> Result<()>;
    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        limit: u32,
        before: Option<MessageId>,
    ) -> Result<Vec<MessagePayload>>;
    async fn send_message(&self, text: &str) -> Result<()>;
    async fn send_message_with_attachment(
        &self,
        text: &str,
        attachment: AttachmentUpload,
    ) -> Result<()>;
    async fn download_file(&self, file_id: FileId) -> Result<Vec<u8>>;
    async fn create_invite(&self, guild_id: GuildId) -> Result<String>;
    async fn join_with_invite(&self, invite_code: &str) -> Result<()>;
    async fn sender_directory(&self) -> HashMap<i64, String>;
    fn subscribe_events(&self) -> broadcast::Receiver<ClientEvent>;
}

pub struct RealtimeClient<C: CryptoProvider + 'static> {
    http: Client,
    _crypto: C,
    mls_session_manager: Arc<dyn MlsSessionManager>,
    inner: Mutex<RealtimeClientState>,
    events: broadcast::Sender<ClientEvent>,
}

struct RealtimeClientState {
    server_url: Option<String>,
    user_id: Option<i64>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,
    ws_started: bool,
    channel_guilds: HashMap<ChannelId, GuildId>,
    sender_directory: HashMap<i64, String>,
}

#[derive(Serialize)]
struct ListMessagesQuery {
    user_id: i64,
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SendMessageHttpRequest {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    ciphertext_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment: Option<AttachmentPayload>,
}

impl<C: CryptoProvider + 'static> RealtimeClient<C> {
    pub fn new(crypto: C) -> Arc<Self> {
        Self::new_with_mls_session_manager(crypto, Arc::new(MissingMlsSessionManager))
    }

    pub fn new_with_mls_session_manager(
        crypto: C,
        mls_session_manager: Arc<dyn MlsSessionManager>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            http: Client::new(),
            _crypto: crypto,
            mls_session_manager,
            inner: Mutex::new(RealtimeClientState {
                server_url: None,
                user_id: None,
                selected_guild: None,
                selected_channel: None,
                ws_started: false,
                channel_guilds: HashMap::new(),
                sender_directory: HashMap::new(),
            }),
            events,
        })
    }

    async fn record_sender_username(&self, message: &MessagePayload) {
        let Some(username) = message.sender_username.clone() else {
            return;
        };

        let mut should_emit = false;
        {
            let mut guard = self.inner.lock().await;
            if guard.sender_directory.get(&message.sender_id.0) != Some(&username) {
                guard
                    .sender_directory
                    .insert(message.sender_id.0, username.clone());
                should_emit = true;
            }
        }

        if should_emit {
            let _ = self.events.send(ClientEvent::UserDirectoryUpdated {
                user_id: message.sender_id.0,
                username,
            });
        }
    }

    async fn spawn_ws_events(self: &Arc<Self>, server_url: &str, user_id: i64) -> Result<()> {
        let ws_url = if server_url.starts_with("https://") {
            server_url.replacen("https://", "wss://", 1)
        } else if server_url.starts_with("http://") {
            server_url.replacen("http://", "ws://", 1)
        } else {
            return Err(anyhow!("server_url must start with http:// or https://"));
        };
        let ws_url = format!("{ws_url}/ws?user_id={user_id}");
        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .with_context(|| format!("failed to connect websocket: {ws_url}"))?;
        let (_, mut ws_reader) = ws_stream.split();

        let client = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(msg) = ws_reader.next().await {
                match msg {
                    Ok(Message::Text(text)) => match serde_json::from_str::<ServerEvent>(&text) {
                        Ok(event) => {
                            if let ServerEvent::MessageReceived { message } = &event {
                                client.record_sender_username(message).await;
                                if let Err(err) = client.emit_decrypted_message(message).await {
                                    let _ = client.events.send(ClientEvent::Error(err.to_string()));
                                }
                            } else {
                                let _ = client.events.send(ClientEvent::Server(event));
                            }
                        }
                        Err(err) => {
                            let _ = client
                                .events
                                .send(ClientEvent::Error(format!("invalid server event: {err}")));
                        }
                    },
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(err) => {
                        let _ = client.events.send(ClientEvent::Error(format!(
                            "websocket receive failed: {err}"
                        )));
                        break;
                    }
                }
            }
            let mut guard = client.inner.lock().await;
            guard.ws_started = false;
        });

        Ok(())
    }

    async fn session(&self) -> Result<(String, i64)> {
        let guard = self.inner.lock().await;
        let server_url = guard
            .server_url
            .clone()
            .ok_or_else(|| anyhow!("not logged in: missing server_url"))?;
        let user_id = guard
            .user_id
            .ok_or_else(|| anyhow!("not logged in: missing user_id"))?;
        Ok((server_url, user_id))
    }

    async fn upload_key_package_for_guild(&self, guild_id: GuildId) -> Result<i64> {
        let (server_url, user_id) = self.session().await?;
        let identity = MlsIdentity::new_with_name(format!("user-{user_id}"))?;
        let key_package_bytes = identity.key_package_bytes(&OpenMlsRustCrypto::default())?;

        let response: UploadKeyPackageResponse = self
            .http
            .post(format!("{server_url}/mls/key_packages"))
            .query(&[("user_id", user_id), ("guild_id", guild_id.0)])
            .body(key_package_bytes)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(response.key_package_id)
    }

    pub async fn fetch_key_package(&self, user_id: i64, guild_id: GuildId) -> Result<Vec<u8>> {
        let (server_url, _current_user_id) = self.session().await?;
        let response: KeyPackageResponse = self
            .http
            .get(format!("{server_url}/mls/key_packages"))
            .query(&[("user_id", user_id), ("guild_id", guild_id.0)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if response.user_id != user_id || response.guild_id != guild_id.0 {
            return Err(anyhow!("server returned mismatched key package metadata"));
        }

        STANDARD
            .decode(response.key_package_b64)
            .map_err(|e| anyhow!("invalid key package payload from server: {e}"))
    }

    fn guild_id_from_invite(invite_code: &str) -> Option<GuildId> {
        let decoded = URL_SAFE_NO_PAD.decode(invite_code.as_bytes()).ok()?;
        let decoded_text = String::from_utf8(decoded).ok()?;
        let guild_id = decoded_text.strip_prefix("guild:")?.parse::<i64>().ok()?;
        Some(GuildId(guild_id))
    }

    async fn upload_attachment(&self, attachment: AttachmentUpload) -> Result<AttachmentPayload> {
        let (server_url, user_id, guild_id, channel_id) = self.active_context().await?;
        let response: FileUploadResponse = self
            .http
            .post(format!("{server_url}/files/upload"))
            .query(&[
                ("user_id", user_id.to_string()),
                ("guild_id", guild_id.0.to_string()),
                ("channel_id", channel_id.0.to_string()),
                ("filename", attachment.filename.clone()),
                (
                    "mime_type",
                    attachment
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                ),
            ])
            .body(attachment.ciphertext)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(AttachmentPayload {
            file_id: FileId(response.file_id),
            filename: attachment.filename,
            size_bytes: response.size_bytes as u64,
            mime_type: attachment.mime_type,
        })
    }

    async fn active_context(&self) -> Result<(String, i64, GuildId, ChannelId)> {
        let guard = self.inner.lock().await;
        let server_url = guard
            .server_url
            .clone()
            .ok_or_else(|| anyhow!("not logged in: missing server_url"))?;
        let user_id = guard
            .user_id
            .ok_or_else(|| anyhow!("not logged in: missing user_id"))?;
        let channel_id = guard
            .selected_channel
            .ok_or_else(|| anyhow!("no channel selected"))?;
        let guild_id = guard
            .selected_guild
            .or_else(|| guard.channel_guilds.get(&channel_id).copied())
            .ok_or_else(|| anyhow!("no guild selected"))?;
        Ok((server_url, user_id, guild_id, channel_id))
    }

    async fn emit_decrypted_message(&self, message: &MessagePayload) -> Result<()> {
        let ciphertext = STANDARD
            .decode(message.ciphertext_b64.as_bytes())
            .with_context(|| {
                format!(
                    "invalid base64 ciphertext for message {}",
                    message.message_id.0
                )
            })?;

        let plaintext_bytes = self
            .mls_session_manager
            .decrypt_application(message.channel_id, &ciphertext)
            .await?;

        if plaintext_bytes.is_empty() {
            return Ok(());
        }

        let plaintext = String::from_utf8_lossy(&plaintext_bytes).to_string();
        let _ = self.events.send(ClientEvent::MessageDecrypted {
            message: message.clone(),
            plaintext,
        });
        Ok(())
    }

    async fn send_message_with_attachment_impl(
        &self,
        text: &str,
        attachment: Option<AttachmentPayload>,
    ) -> Result<()> {
        let (server_url, user_id, guild_id, channel_id) = self.active_context().await?;
        let plaintext_bytes = text.as_bytes();
        let ciphertext = self
            .mls_session_manager
            .encrypt_application(channel_id, plaintext_bytes)
            .await?;
        let payload = SendMessageHttpRequest {
            user_id,
            guild_id: guild_id.0,
            channel_id: channel_id.0,
            ciphertext_b64: STANDARD.encode(ciphertext),
            attachment,
        };

        self.http
            .post(format!("{server_url}/messages"))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl<C: CryptoProvider + 'static> ClientHandle for Arc<RealtimeClient<C>> {
    async fn login(
        &self,
        server_url: &str,
        username: &str,
        _password_or_invite: &str,
    ) -> Result<()> {
        let res = self
            .http
            .post(format!("{server_url}/login"))
            .json(&LoginRequest {
                username: username.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        let body: LoginResponse = res.json().await?;

        {
            let mut guard = self.inner.lock().await;
            guard.server_url = Some(server_url.to_string());
            guard.user_id = Some(body.user_id);
            guard.selected_guild = None;
            guard.selected_channel = None;
            guard.ws_started = true;
            guard.channel_guilds.clear();
            guard.sender_directory.clear();
        }

        self.spawn_ws_events(server_url, body.user_id).await?;

        let guilds: Vec<GuildSummary> = self
            .http
            .get(format!("{server_url}/guilds"))
            .query(&[("user_id", body.user_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        for guild in guilds {
            self.upload_key_package_for_guild(guild.guild_id).await?;
        }

        Ok(())
    }

    async fn list_guilds(&self) -> Result<()> {
        let (server_url, user_id) = self.session().await?;
        let guilds: Vec<GuildSummary> = self
            .http
            .get(format!("{server_url}/guilds"))
            .query(&[("user_id", user_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        for guild in guilds {
            let _ = self
                .events
                .send(ClientEvent::Server(ServerEvent::GuildUpdated { guild }));
        }
        Ok(())
    }

    async fn list_channels(&self, guild_id: GuildId) -> Result<()> {
        let (server_url, user_id) = self.session().await?;
        {
            let mut guard = self.inner.lock().await;
            guard.selected_guild = Some(guild_id);
        }

        let channels: Vec<ChannelSummary> = self
            .http
            .get(format!("{server_url}/guilds/{}/channels", guild_id.0))
            .query(&[("user_id", user_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        {
            let mut guard = self.inner.lock().await;
            for channel in &channels {
                guard
                    .channel_guilds
                    .insert(channel.channel_id, channel.guild_id);
            }
        }

        for channel in channels {
            let _ = self
                .events
                .send(ClientEvent::Server(ServerEvent::ChannelUpdated { channel }));
        }

        self.list_members(guild_id).await?;
        Ok(())
    }

    async fn list_members(&self, guild_id: GuildId) -> Result<Vec<MemberSummary>> {
        let (server_url, user_id) = self.session().await?;

        let members: Vec<MemberSummary> = self
            .http
            .get(format!("{server_url}/guilds/{}/members", guild_id.0))
            .query(&[("user_id", user_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let _ = self
            .events
            .send(ClientEvent::Server(ServerEvent::GuildMembersUpdated {
                guild_id,
                members: members.clone(),
            }));

        Ok(members)
    }

    async fn select_channel(&self, channel_id: ChannelId) -> Result<()> {
        {
            let mut guard = self.inner.lock().await;
            guard.selected_channel = Some(channel_id);
            if let Some(guild_id) = guard.channel_guilds.get(&channel_id).copied() {
                guard.selected_guild = Some(guild_id);
            }
        }
        self.fetch_messages(channel_id, 100, None).await?;
        Ok(())
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        limit: u32,
        before: Option<MessageId>,
    ) -> Result<Vec<MessagePayload>> {
        let (server_url, user_id) = self.session().await?;
        let limit = limit.clamp(1, 100);
        let messages: Vec<MessagePayload> = self
            .http
            .get(format!("{server_url}/channels/{}/messages", channel_id.0))
            .query(&ListMessagesQuery {
                user_id,
                limit,
                before: before.map(|id| id.0),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        for message in &messages {
            self.record_sender_username(message).await;
            if let Err(err) = self.emit_decrypted_message(message).await {
                let _ = self.events.send(ClientEvent::Error(err.to_string()));
            }
        }

        Ok(messages)
    }

    async fn send_message(&self, text: &str) -> Result<()> {
        self.send_message_with_attachment_impl(text, None).await
    }

    async fn send_message_with_attachment(
        &self,
        text: &str,
        attachment: AttachmentUpload,
    ) -> Result<()> {
        let uploaded = self.upload_attachment(attachment).await?;
        self.send_message_with_attachment_impl(text, Some(uploaded))
            .await
    }

    async fn download_file(&self, file_id: FileId) -> Result<Vec<u8>> {
        let (server_url, user_id) = self.session().await?;
        let bytes = self
            .http
            .get(format!("{server_url}/files/{}", file_id.0))
            .query(&[("user_id", user_id)])
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    async fn create_invite(&self, guild_id: GuildId) -> Result<String> {
        let (server_url, user_id) = self.session().await?;
        let response: InviteResponse = self
            .http
            .post(format!("{server_url}/guilds/{}/invites", guild_id.0))
            .query(&[("user_id", user_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(response.invite_code)
    }

    async fn join_with_invite(&self, invite_code: &str) -> Result<()> {
        let (server_url, user_id) = self.session().await?;
        self.http
            .post(format!("{server_url}/guilds/join"))
            .json(&JoinGuildRequest {
                user_id,
                invite_code: invite_code.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;

        if let Some(guild_id) = RealtimeClient::<C>::guild_id_from_invite(invite_code) {
            self.upload_key_package_for_guild(guild_id).await?;
        }

        Ok(())
    }

    async fn sender_directory(&self) -> HashMap<i64, String> {
        let guard = self.inner.lock().await;
        guard.sender_directory.clone()
    }

    fn subscribe_events(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post, Json, Router};
    use tokio::{net::TcpListener, sync::oneshot};

    #[derive(Clone)]
    struct ServerState {
        tx: Arc<Mutex<Option<oneshot::Sender<SendMessageHttpRequest>>>>,
    }

    struct TestMlsSessionManager {
        encrypt_ciphertext: Vec<u8>,
        decrypt_plaintext: Vec<u8>,
        fail_with: Option<String>,
    }

    impl TestMlsSessionManager {
        fn ok(encrypt_ciphertext: Vec<u8>, decrypt_plaintext: Vec<u8>) -> Self {
            Self {
                encrypt_ciphertext,
                decrypt_plaintext,
                fail_with: None,
            }
        }

        fn failing(err: impl Into<String>) -> Self {
            Self {
                encrypt_ciphertext: Vec::new(),
                decrypt_plaintext: Vec::new(),
                fail_with: Some(err.into()),
            }
        }
    }

    #[async_trait]
    impl MlsSessionManager for TestMlsSessionManager {
        async fn encrypt_application(
            &self,
            _channel_id: ChannelId,
            plaintext: &[u8],
        ) -> Result<Vec<u8>> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            if plaintext.is_empty() {
                return Err(anyhow!("plaintext must not be empty"));
            }
            Ok(self.encrypt_ciphertext.clone())
        }

        async fn decrypt_application(
            &self,
            _channel_id: ChannelId,
            _ciphertext: &[u8],
        ) -> Result<Vec<u8>> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            Ok(self.decrypt_plaintext.clone())
        }
    }

    async fn handle_send_message(
        State(state): State<ServerState>,
        Json(payload): Json<SendMessageHttpRequest>,
    ) {
        if let Some(tx) = state.tx.lock().await.take() {
            let _ = tx.send(payload);
        }
    }

    async fn spawn_message_server() -> Result<(String, oneshot::Receiver<SendMessageHttpRequest>)> {
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (tx, rx) = oneshot::channel();
        let state = ServerState {
            tx: Arc::new(Mutex::new(Some(tx))),
        };
        let app = Router::new()
            .route("/messages", post(handle_send_message))
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{addr}"), rx))
    }

    #[tokio::test]
    async fn send_message_uses_mls_ciphertext_payload() {
        let (server_url, payload_rx) = spawn_message_server().await.expect("spawn server");
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(
                b"mls-ciphertext".to_vec(),
                Vec::new(),
            )),
        );

        {
            let mut inner = client.inner.lock().await;
            inner.server_url = Some(server_url);
            inner.user_id = Some(7);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
        }

        client
            .send_message("plaintext-message")
            .await
            .expect("send");

        let payload = payload_rx.await.expect("payload");
        let plaintext_b64 = STANDARD.encode("plaintext-message".as_bytes());
        assert_ne!(payload.ciphertext_b64, plaintext_b64);
        assert_eq!(
            payload.ciphertext_b64,
            STANDARD.encode("mls-ciphertext".as_bytes())
        );
    }

    #[tokio::test]
    async fn send_message_requires_active_mls_state() {
        let (server_url, _payload_rx) = spawn_message_server().await.expect("spawn server");
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::failing("group not initialized")),
        );

        {
            let mut inner = client.inner.lock().await;
            inner.server_url = Some(server_url);
            inner.user_id = Some(7);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
        }

        let err = client
            .send_message("plaintext-message")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("group not initialized"));
    }

    fn sample_message() -> MessagePayload {
        MessagePayload {
            message_id: MessageId(7),
            channel_id: ChannelId(3),
            sender_id: shared::domain::UserId(5),
            sender_username: Some("alice".to_string()),
            ciphertext_b64: STANDARD.encode(b"cipher"),
            attachment: None,
            sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
        }
    }

    #[tokio::test]
    async fn emits_decrypted_message_event_for_application_data() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec())),
        );
        let mut rx = client.subscribe_events();

        client
            .emit_decrypted_message(&sample_message())
            .await
            .expect("decrypt should succeed");

        let event = rx.recv().await.expect("event");
        match event {
            ClientEvent::MessageDecrypted { plaintext, .. } => assert_eq!(plaintext, "hello"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn suppresses_non_application_messages_after_decrypt() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(Vec::new(), Vec::new())),
        );
        let mut rx = client.subscribe_events();

        client
            .emit_decrypted_message(&sample_message())
            .await
            .expect("decrypt should still succeed");

        assert!(rx.try_recv().is_err());
    }
}
