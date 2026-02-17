use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures::StreamExt;
use livekit_integration::{
    LiveKitRoomConnector, LiveKitRoomEvent, LiveKitRoomOptions, LiveKitRoomSession, LocalTrack,
};
use mls::MlsIdentity;
use openmls_rust_crypto::OpenMlsRustCrypto;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, FileId, GuildId, MessageId},
    protocol::{
        AttachmentPayload, ChannelSummary, ClientRequest, GuildSummary, KeyPackageResponse,
        MemberSummary, MessagePayload, ServerEvent, UploadKeyPackageResponse, WelcomeResponse,
    },
};
use thiserror::Error;
use tokio::{
    sync::{broadcast, Mutex, RwLock},
    task::JoinHandle,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use zeroize::Zeroize;

mod mls_session_manager;
pub use mls_session_manager::DurableMlsSessionManager;

const LIVEKIT_E2EE_EXPORT_LABEL: &str = "livekit-e2ee";
const LIVEKIT_E2EE_KEY_LEN: usize = 32;
const LIVEKIT_E2EE_CACHE_TTL: Duration = Duration::from_secs(90);
const LIVEKIT_E2EE_INFO_PREFIX: &[u8] = b"proto-rtc/livekit-e2ee/v1";
/// Deterministic application salt for LiveKit E2EE key derivation.
const LIVEKIT_E2EE_APP_SALT: &[u8] = b"proto-rtc/livekit-e2ee-app-salt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct VoiceConnectionKey {
    guild_id: GuildId,
    channel_id: ChannelId,
}

impl VoiceConnectionKey {
    fn new(guild_id: GuildId, channel_id: ChannelId) -> Self {
        Self {
            guild_id,
            channel_id,
        }
    }
}

struct CachedVoiceSessionKey {
    key: Vec<u8>,
    expires_at: Instant,
}

#[derive(Debug, Error)]
pub enum LiveKitE2eeKeyError {
    #[error("missing MLS group for guild {guild_id} channel {channel_id}")]
    MissingMlsGroup { guild_id: i64, channel_id: i64 },
    #[error("MLS secret export failed for guild {guild_id} channel {channel_id}: {source}")]
    ExportFailure {
        guild_id: i64,
        channel_id: i64,
        source: anyhow::Error,
    },
    #[error("invalid derived LiveKit E2EE key length: expected {expected}, got {actual}")]
    InvalidDerivedKeyLength { expected: usize, actual: usize },
}

#[async_trait]
pub trait MlsSessionManager: Send + Sync {
    async fn open_or_create_group(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()>;
    async fn encrypt_application(&self, channel_id: ChannelId, plaintext: &[u8])
        -> Result<Vec<u8>>;
    async fn decrypt_application(
        &self,
        channel_id: ChannelId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>>;
    async fn add_member(&self, channel_id: ChannelId, key_package_bytes: &[u8]) -> Result<Vec<u8>>;
    async fn join_from_welcome(&self, channel_id: ChannelId, welcome_bytes: &[u8]) -> Result<()>;
    async fn export_secret(
        &self,
        channel_id: ChannelId,
        label: &str,
        len: usize,
    ) -> Result<Vec<u8>>;
}

pub struct MissingMlsSessionManager;

#[async_trait]
impl MlsSessionManager for MissingMlsSessionManager {
    async fn open_or_create_group(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
        Err(anyhow!(
            "MLS backend unavailable for guild {} channel {}",
            guild_id.0,
            channel_id.0
        ))
    }

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

    async fn add_member(
        &self,
        channel_id: ChannelId,
        _key_package_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }

    async fn join_from_welcome(&self, channel_id: ChannelId, _welcome_bytes: &[u8]) -> Result<()> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }

    async fn export_secret(
        &self,
        channel_id: ChannelId,
        _label: &str,
        _len: usize,
    ) -> Result<Vec<u8>> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceParticipantState {
    pub participant_id: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSessionSnapshot {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub room_name: String,
    pub e2ee_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct VoiceConnectOptions {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub can_publish_mic: bool,
    pub can_publish_screen: bool,
}

#[derive(Debug, Error)]
pub enum VoiceSessionError {
    #[error("failed to request voice token: {0}")]
    TokenRequest(String),
    #[error("unexpected server response for voice token request")]
    UnexpectedTokenResponse,
    #[error("voice token response channel mismatch")]
    TokenChannelMismatch,
    #[error("failed to derive e2ee key: {0}")]
    E2eeKey(#[from] LiveKitE2eeKeyError),
    #[error("failed to connect livekit room: {0}")]
    Connect(String),
}

#[async_trait]
pub trait LiveKitControlPlane: Send + Sync {
    async fn request_livekit_token(&self, request: ClientRequest) -> Result<ServerEvent>;
}

pub struct MissingLiveKitControlPlane;

#[async_trait]
impl LiveKitControlPlane for MissingLiveKitControlPlane {
    async fn request_livekit_token(&self, _request: ClientRequest) -> Result<ServerEvent> {
        Err(anyhow!("livekit control plane is unavailable"))
    }
}

#[async_trait]
pub trait LiveKitConnectorProvider: Send + Sync {
    async fn connect_room(
        &self,
        options: LiveKitRoomOptions,
    ) -> Result<Arc<dyn LiveKitRoomSession>>;
}

pub struct MissingLiveKitConnector;

#[async_trait]
impl LiveKitConnectorProvider for MissingLiveKitConnector {
    async fn connect_room(
        &self,
        _options: LiveKitRoomOptions,
    ) -> Result<Arc<dyn LiveKitRoomSession>> {
        Err(anyhow!("livekit connector is unavailable"))
    }
}

#[async_trait]
impl<T> LiveKitConnectorProvider for T
where
    T: LiveKitRoomConnector,
{
    async fn connect_room(
        &self,
        options: LiveKitRoomOptions,
    ) -> Result<Arc<dyn LiveKitRoomSession>> {
        self.connect(options).await
    }
}

pub trait CryptoProvider: Send + Sync {
    fn encrypt_message(&self, plaintext: &[u8]) -> Vec<u8>;
    fn decrypt_message(&self, ciphertext: &[u8]) -> Vec<u8>;
}

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
    VoiceSessionStateChanged(Option<VoiceSessionSnapshot>),
    VoiceParticipantsUpdated {
        guild_id: GuildId,
        channel_id: ChannelId,
        participants: Vec<VoiceParticipantState>,
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
    async fn connect_voice_session(&self, options: VoiceConnectOptions) -> Result<()>;
    async fn disconnect_voice_session(&self) -> Result<()>;
    fn subscribe_events(&self) -> broadcast::Receiver<ClientEvent>;
}

pub struct RealtimeClient<C: CryptoProvider + 'static> {
    http: Client,
    _crypto: C,
    mls_session_manager: Arc<dyn MlsSessionManager>,
    livekit_control_plane: Arc<dyn LiveKitControlPlane>,
    livekit_connector: Arc<dyn LiveKitConnectorProvider>,
    inner: Mutex<RealtimeClientState>,
    voice_connection: Mutex<Option<ActiveVoiceSession>>,
    voice_participants: RwLock<HashMap<String, VoiceParticipantState>>,
    events: broadcast::Sender<ClientEvent>,
}

struct ActiveVoiceSession {
    snapshot: VoiceSessionSnapshot,
    room: Arc<dyn LiveKitRoomSession>,
    event_task: JoinHandle<()>,
}

struct RealtimeClientState {
    server_url: Option<String>,
    user_id: Option<i64>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,
    ws_started: bool,
    channel_guilds: HashMap<ChannelId, GuildId>,
    sender_directory: HashMap<i64, String>,
    attempted_channel_member_additions: HashSet<(GuildId, ChannelId, i64)>,
    voice_session_keys: HashMap<VoiceConnectionKey, CachedVoiceSessionKey>,
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
        Self::new_with_dependencies(
            crypto,
            Arc::new(MissingMlsSessionManager),
            Arc::new(MissingLiveKitControlPlane),
            Arc::new(MissingLiveKitConnector),
        )
    }

    pub fn new_with_mls_session_manager(
        crypto: C,
        mls_session_manager: Arc<dyn MlsSessionManager>,
    ) -> Arc<Self> {
        Self::new_with_dependencies(
            crypto,
            mls_session_manager,
            Arc::new(MissingLiveKitControlPlane),
            Arc::new(MissingLiveKitConnector),
        )
    }

    pub fn new_with_dependencies(
        crypto: C,
        mls_session_manager: Arc<dyn MlsSessionManager>,
        livekit_control_plane: Arc<dyn LiveKitControlPlane>,
        livekit_connector: Arc<dyn LiveKitConnectorProvider>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            http: Client::new(),
            _crypto: crypto,
            mls_session_manager,
            livekit_control_plane,
            livekit_connector,
            inner: Mutex::new(RealtimeClientState {
                server_url: None,
                user_id: None,
                selected_guild: None,
                selected_channel: None,
                ws_started: false,
                channel_guilds: HashMap::new(),
                sender_directory: HashMap::new(),
                attempted_channel_member_additions: HashSet::new(),
                voice_session_keys: HashMap::new(),
            }),
            voice_connection: Mutex::new(None),
            voice_participants: RwLock::new(HashMap::new()),
            events,
        })
    }

    pub async fn derive_livekit_e2ee_key(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> std::result::Result<[u8; LIVEKIT_E2EE_KEY_LEN], LiveKitE2eeKeyError> {
        let cache_key = VoiceConnectionKey::new(guild_id, channel_id);
        {
            let mut guard = self.inner.lock().await;
            if let Some(cached) = guard.voice_session_keys.get_mut(&cache_key) {
                if cached.expires_at > Instant::now() {
                    if cached.key.len() != LIVEKIT_E2EE_KEY_LEN {
                        let actual = cached.key.len();
                        cached.key.zeroize();
                        guard.voice_session_keys.remove(&cache_key);
                        return Err(LiveKitE2eeKeyError::InvalidDerivedKeyLength {
                            expected: LIVEKIT_E2EE_KEY_LEN,
                            actual,
                        });
                    }
                    let mut key = [0u8; LIVEKIT_E2EE_KEY_LEN];
                    key.copy_from_slice(&cached.key);
                    return Ok(key);
                }
                cached.key.zeroize();
                guard.voice_session_keys.remove(&cache_key);
            }
        }

        let mut exported = self
            .mls_session_manager
            .export_secret(channel_id, LIVEKIT_E2EE_EXPORT_LABEL, LIVEKIT_E2EE_KEY_LEN)
            .await
            .map_err(|source| map_export_error(guild_id, channel_id, source))?;

        let mut info = build_livekit_hkdf_info(guild_id, channel_id);
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(LIVEKIT_E2EE_APP_SALT), &exported);
        let mut okm = vec![0u8; LIVEKIT_E2EE_KEY_LEN];
        hk.expand(&info, &mut okm)
            .map_err(|_| LiveKitE2eeKeyError::ExportFailure {
                guild_id: guild_id.0,
                channel_id: channel_id.0,
                source: anyhow!("hkdf expansion failed"),
            })?;

        exported.zeroize();
        info.zeroize();

        if okm.len() != LIVEKIT_E2EE_KEY_LEN {
            let actual = okm.len();
            okm.zeroize();
            return Err(LiveKitE2eeKeyError::InvalidDerivedKeyLength {
                expected: LIVEKIT_E2EE_KEY_LEN,
                actual,
            });
        }

        let mut key = [0u8; LIVEKIT_E2EE_KEY_LEN];
        key.copy_from_slice(&okm);
        okm.zeroize();

        let mut guard = self.inner.lock().await;
        guard.voice_session_keys.insert(
            cache_key,
            CachedVoiceSessionKey {
                key: key.to_vec(),
                expires_at: Instant::now() + LIVEKIT_E2EE_CACHE_TTL,
            },
        );

        Ok(key)
    }

    pub async fn clear_voice_session_key(&self, guild_id: GuildId, channel_id: ChannelId) {
        let mut guard = self.inner.lock().await;
        if let Some(mut cached) = guard
            .voice_session_keys
            .remove(&VoiceConnectionKey::new(guild_id, channel_id))
        {
            cached.key.zeroize();
        }
    }

    async fn spawn_voice_event_task(
        self: &Arc<Self>,
        guild_id: GuildId,
        channel_id: ChannelId,
        room: Arc<dyn LiveKitRoomSession>,
    ) -> JoinHandle<()> {
        let mut events = room.subscribe_events();
        let client = Arc::clone(self);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                let snapshot = {
                    let mut participants = client.voice_participants.write().await;
                    match event {
                        LiveKitRoomEvent::ParticipantJoined(participant) => {
                            participants.insert(
                                participant.participant_id.clone(),
                                VoiceParticipantState {
                                    participant_id: participant.participant_id,
                                    identity: participant.identity,
                                },
                            );
                        }
                        LiveKitRoomEvent::ParticipantLeft { participant_id } => {
                            participants.remove(&participant_id);
                        }
                    }
                    participants.values().cloned().collect::<Vec<_>>()
                };

                let _ = client.events.send(ClientEvent::VoiceParticipantsUpdated {
                    guild_id,
                    channel_id,
                    participants: snapshot,
                });
            }
        })
    }

    pub async fn connect_voice_session(
        self: &Arc<Self>,
        options: VoiceConnectOptions,
    ) -> Result<()> {
        let request = ClientRequest::RequestLiveKitToken {
            guild_id: options.guild_id,
            channel_id: options.channel_id,
            can_publish_mic: options.can_publish_mic,
            can_publish_screen: options.can_publish_screen,
        };

        let event = self
            .livekit_control_plane
            .request_livekit_token(request)
            .await
            .map_err(|err| VoiceSessionError::TokenRequest(err.to_string()))?;

        let (guild_id, channel_id, room_name, token) = match event {
            ServerEvent::LiveKitTokenIssued {
                guild_id,
                channel_id,
                room_name,
                token,
            } => (guild_id, channel_id, room_name, token),
            _ => return Err(VoiceSessionError::UnexpectedTokenResponse.into()),
        };

        if guild_id != options.guild_id || channel_id != options.channel_id {
            return Err(VoiceSessionError::TokenChannelMismatch.into());
        }

        let e2ee_key = self
            .derive_livekit_e2ee_key(guild_id, channel_id)
            .await
            .map_err(VoiceSessionError::E2eeKey)?;

        let room = self
            .livekit_connector
            .connect_room(LiveKitRoomOptions {
                room_name: room_name.clone(),
                token,
                e2ee_key: e2ee_key.to_vec(),
                e2ee_enabled: true,
            })
            .await
            .map_err(|err| VoiceSessionError::Connect(err.to_string()))?;

        if options.can_publish_mic {
            room.publish_local_track(LocalTrack::Microphone).await?;
        }

        if options.can_publish_screen {
            if room.supports_screen_share() {
                room.publish_local_track(LocalTrack::ScreenShare).await?;
            } else {
                warn!("voice: screen share not supported by room backend room={room_name}");
            }
        }

        if !room.is_e2ee_enabled() {
            error!(
                "voice: e2ee enabled=false downgraded=true room={} guild={} channel={}",
                room_name, guild_id.0, channel_id.0
            );
        } else {
            info!(
                "voice: e2ee enabled=true room={} guild={} channel={}",
                room_name, guild_id.0, channel_id.0
            );
        }

        let snapshot = VoiceSessionSnapshot {
            guild_id,
            channel_id,
            room_name,
            e2ee_enabled: room.is_e2ee_enabled(),
        };

        let task = self
            .spawn_voice_event_task(guild_id, channel_id, Arc::clone(&room))
            .await;

        {
            let mut participants = self.voice_participants.write().await;
            participants.clear();
        }

        let previous = {
            let mut voice = self.voice_connection.lock().await;
            voice.replace(ActiveVoiceSession {
                snapshot: snapshot.clone(),
                room,
                event_task: task,
            })
        };
        if let Some(active) = previous {
            active.event_task.abort();
        }

        let _ = self
            .events
            .send(ClientEvent::VoiceSessionStateChanged(Some(snapshot)));

        Ok(())
    }

    pub async fn disconnect_voice_session(&self) -> Result<()> {
        let active = {
            let mut guard = self.voice_connection.lock().await;
            guard.take()
        };

        if let Some(active) = active {
            let _ = active.room.unpublish_local_tracks().await;
            let _ = active.room.leave().await;
            active.event_task.abort();
            self.clear_voice_session_key(active.snapshot.guild_id, active.snapshot.channel_id)
                .await;
        }

        self.voice_participants.write().await.clear();
        let _ = self
            .events
            .send(ClientEvent::VoiceSessionStateChanged(None));

        Ok(())
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
            zeroize_voice_session_cache(&mut guard);
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

    async fn store_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        target_user_id: i64,
        welcome_bytes: &[u8],
    ) -> Result<()> {
        let (server_url, current_user_id) = self.session().await?;
        self.http
            .post(format!("{server_url}/mls/welcome"))
            .query(&[
                ("user_id", current_user_id),
                ("guild_id", guild_id.0),
                ("channel_id", channel_id.0),
                ("target_user_id", target_user_id),
            ])
            .body(welcome_bytes.to_vec())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn maybe_join_from_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<()> {
        let (server_url, user_id) = self.session().await?;
        let response = self
            .http
            .get(format!("{server_url}/mls/welcome"))
            .query(&[
                ("user_id", user_id),
                ("guild_id", guild_id.0),
                ("channel_id", channel_id.0),
            ])
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }

        let response = response.error_for_status()?;
        let welcome: WelcomeResponse = response.json().await?;
        let welcome_bytes = STANDARD
            .decode(welcome.welcome_b64)
            .map_err(|e| anyhow!("invalid welcome payload from server: {e}"))?;
        self.mls_session_manager
            .open_or_create_group(guild_id, channel_id)
            .await?;
        self.mls_session_manager
            .join_from_welcome(channel_id, &welcome_bytes)
            .await
    }

    async fn fetch_members_for_guild(&self, guild_id: GuildId) -> Result<Vec<MemberSummary>> {
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
        Ok(members)
    }

    async fn maybe_add_existing_members_to_channel_group(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<()> {
        let (_server_url, current_user_id) = self.session().await?;
        self.mls_session_manager
            .open_or_create_group(guild_id, channel_id)
            .await?;
        let members = self.fetch_members_for_guild(guild_id).await?;

        for member in members {
            if member.user_id.0 == current_user_id {
                continue;
            }

            let should_attempt = {
                let mut guard = self.inner.lock().await;
                guard.attempted_channel_member_additions.insert((
                    guild_id,
                    channel_id,
                    member.user_id.0,
                ))
            };
            if !should_attempt {
                continue;
            }

            let key_package_bytes = match self.fetch_key_package(member.user_id.0, guild_id).await {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let welcome_bytes = match self
                .mls_session_manager
                .add_member(channel_id, &key_package_bytes)
                .await
            {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let _ = self
                .store_pending_welcome(guild_id, channel_id, member.user_id.0, &welcome_bytes)
                .await;
        }

        Ok(())
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
        let guild_id = {
            let mut guard = self.inner.lock().await;
            if let Some(guild_id) = guard.channel_guilds.get(&message.channel_id).copied() {
                guild_id
            } else if guard.selected_channel == Some(message.channel_id) {
                if let Some(guild_id) = guard.selected_guild {
                    guard.channel_guilds.insert(message.channel_id, guild_id);
                    guild_id
                } else {
                    return Err(anyhow!(
                        "missing guild mapping for channel {}",
                        message.channel_id.0
                    ));
                }
            } else {
                return Err(anyhow!(
                    "missing guild mapping for channel {}",
                    message.channel_id.0
                ));
            }
        };
        self.mls_session_manager
            .open_or_create_group(guild_id, message.channel_id)
            .await?;
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
        self.mls_session_manager
            .open_or_create_group(guild_id, channel_id)
            .await?;
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

fn build_livekit_hkdf_info(guild_id: GuildId, channel_id: ChannelId) -> Vec<u8> {
    let mut info = Vec::with_capacity(LIVEKIT_E2EE_INFO_PREFIX.len() + 1 + 16);
    info.extend_from_slice(LIVEKIT_E2EE_INFO_PREFIX);
    info.push(0);
    info.extend_from_slice(&guild_id.0.to_be_bytes());
    info.extend_from_slice(&channel_id.0.to_be_bytes());
    info
}

fn zeroize_voice_session_cache(state: &mut RealtimeClientState) {
    for cached in state.voice_session_keys.values_mut() {
        cached.key.zeroize();
    }
    state.voice_session_keys.clear();
}

fn map_export_error(
    guild_id: GuildId,
    channel_id: ChannelId,
    source: anyhow::Error,
) -> LiveKitE2eeKeyError {
    let msg = source.to_string();
    if is_missing_mls_group_error(&msg) {
        LiveKitE2eeKeyError::MissingMlsGroup {
            guild_id: guild_id.0,
            channel_id: channel_id.0,
        }
    } else {
        LiveKitE2eeKeyError::ExportFailure {
            guild_id: guild_id.0,
            channel_id: channel_id.0,
            source,
        }
    }
}

fn is_missing_mls_group_error(message: &str) -> bool {
    message.contains("no active MLS group")
        || message.contains("group not initialized")
        || message.contains("MLS group not opened")
        || message.contains("MLS session missing")
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
            zeroize_voice_session_cache(&mut guard);
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
        let members = self.fetch_members_for_guild(guild_id).await?;

        let _ = self
            .events
            .send(ClientEvent::Server(ServerEvent::GuildMembersUpdated {
                guild_id,
                members: members.clone(),
            }));

        Ok(members)
    }

    async fn select_channel(&self, channel_id: ChannelId) -> Result<()> {
        let guild_id = {
            let mut guard = self.inner.lock().await;
            guard.selected_channel = Some(channel_id);
            if let Some(guild_id) = guard.channel_guilds.get(&channel_id).copied() {
                guard.selected_guild = Some(guild_id);
            }
            guard.selected_guild
        }
        .ok_or_else(|| anyhow!("no guild selected"))?;

        // MVP trigger: perform add-member fanout on the first interaction with a channel after join.
        self.maybe_join_from_pending_welcome(guild_id, channel_id)
            .await?;
        self.maybe_add_existing_members_to_channel_group(guild_id, channel_id)
            .await?;

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
        {
            let mut guard = self.inner.lock().await;
            if !guard.channel_guilds.contains_key(&channel_id) {
                if let Some(guild_id) = guard.selected_guild {
                    guard.channel_guilds.insert(channel_id, guild_id);
                }
            }
        }
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

    async fn connect_voice_session(&self, options: VoiceConnectOptions) -> Result<()> {
        RealtimeClient::connect_voice_session(self, options).await
    }

    async fn disconnect_voice_session(&self) -> Result<()> {
        RealtimeClient::disconnect_voice_session(self).await
    }

    fn subscribe_events(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Query, State},
        http::StatusCode,
        routing::post,
        Json, Router,
    };
    use tokio::{net::TcpListener, sync::oneshot};

    #[derive(Clone)]
    struct ServerState {
        tx: Arc<Mutex<Option<oneshot::Sender<SendMessageHttpRequest>>>>,
    }

    struct TestMlsSessionManager {
        encrypt_ciphertext: Vec<u8>,
        decrypt_plaintext: Vec<u8>,
        add_member_welcome: Vec<u8>,
        fail_with: Option<String>,
        exported_secret: Vec<u8>,
        joined_welcomes: Arc<Mutex<Vec<Vec<u8>>>>,
        decrypted_ciphertexts: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TestMlsSessionManager {
        fn ok(encrypt_ciphertext: Vec<u8>, decrypt_plaintext: Vec<u8>) -> Self {
            Self {
                encrypt_ciphertext,
                decrypt_plaintext,
                add_member_welcome: b"welcome-generated".to_vec(),
                fail_with: None,
                exported_secret: b"mls-export-secret".to_vec(),
                joined_welcomes: Arc::new(Mutex::new(Vec::new())),
                decrypted_ciphertexts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing(err: impl Into<String>) -> Self {
            Self {
                encrypt_ciphertext: Vec::new(),
                decrypt_plaintext: Vec::new(),
                add_member_welcome: Vec::new(),
                fail_with: Some(err.into()),
                exported_secret: Vec::new(),
                joined_welcomes: Arc::new(Mutex::new(Vec::new())),
                decrypted_ciphertexts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_exported_secret(secret: Vec<u8>) -> Self {
            let mut manager = Self::ok(Vec::new(), Vec::new());
            manager.exported_secret = secret;
            manager
        }
    }

    #[async_trait]
    impl MlsSessionManager for TestMlsSessionManager {
        async fn open_or_create_group(
            &self,
            _guild_id: GuildId,
            _channel_id: ChannelId,
        ) -> Result<()> {
            Ok(())
        }

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
            ciphertext: &[u8],
        ) -> Result<Vec<u8>> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            self.decrypted_ciphertexts
                .lock()
                .await
                .push(ciphertext.to_vec());
            Ok(self.decrypt_plaintext.clone())
        }

        async fn add_member(
            &self,
            _channel_id: ChannelId,
            _key_package_bytes: &[u8],
        ) -> Result<Vec<u8>> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            Ok(self.add_member_welcome.clone())
        }

        async fn join_from_welcome(
            &self,
            _channel_id: ChannelId,
            welcome_bytes: &[u8],
        ) -> Result<()> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            self.joined_welcomes
                .lock()
                .await
                .push(welcome_bytes.to_vec());
            Ok(())
        }

        async fn export_secret(
            &self,
            _channel_id: ChannelId,
            _label: &str,
            _len: usize,
        ) -> Result<Vec<u8>> {
            if let Some(err) = &self.fail_with {
                return Err(anyhow!(err.clone()));
            }
            Ok(self.exported_secret.clone())
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

    #[test]
    fn classify_missing_mls_group_errors_from_all_backends() {
        assert!(is_missing_mls_group_error(
            "no active MLS group available for channel 9"
        ));
        assert!(is_missing_mls_group_error(
            "MLS group not opened for channel 9"
        ));
        assert!(is_missing_mls_group_error(
            "MLS session missing for guild 1 channel 9"
        ));
        assert!(is_missing_mls_group_error("MLS group not initialized"));
        assert!(!is_missing_mls_group_error("failed to connect to server"));
    }

    #[tokio::test]
    async fn emit_decrypted_message_backfills_channel_mapping_from_selected_context() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec())),
        );
        {
            let mut inner = client.inner.lock().await;
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(3));
        }

        client
            .emit_decrypted_message(&sample_message())
            .await
            .expect("decrypt should succeed with selected channel fallback");

        let inner = client.inner.lock().await;
        assert_eq!(inner.channel_guilds.get(&ChannelId(3)), Some(&GuildId(11)));
    }

    #[tokio::test]
    async fn emits_decrypted_message_event_for_application_data() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec())),
        );
        {
            let mut inner = client.inner.lock().await;
            inner.channel_guilds.insert(ChannelId(3), GuildId(11));
        }
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
        {
            let mut inner = client.inner.lock().await;
            inner.channel_guilds.insert(ChannelId(3), GuildId(11));
        }
        let mut rx = client.subscribe_events();

        client
            .emit_decrypted_message(&sample_message())
            .await
            .expect("decrypt should still succeed");

        assert!(rx.try_recv().is_err());
    }

    #[derive(Clone)]
    struct OnboardingServerState {
        pending_welcome_b64: Arc<Mutex<Option<String>>>,
        add_member_posts: Arc<Mutex<Vec<(i64, i64, i64)>>>,
        stored_ciphertexts: Arc<Mutex<Vec<String>>>,
    }

    async fn onboarding_list_members() -> Json<Vec<MemberSummary>> {
        Json(vec![
            MemberSummary {
                guild_id: GuildId(11),
                user_id: shared::domain::UserId(7),
                username: "adder".to_string(),
                role: shared::domain::Role::Owner,
                muted: false,
            },
            MemberSummary {
                guild_id: GuildId(11),
                user_id: shared::domain::UserId(42),
                username: "target".to_string(),
                role: shared::domain::Role::Member,
                muted: false,
            },
        ])
    }

    async fn onboarding_fetch_key_package() -> Json<KeyPackageResponse> {
        Json(KeyPackageResponse {
            key_package_id: 1,
            guild_id: 11,
            user_id: 42,
            key_package_b64: STANDARD.encode(b"target-kp"),
        })
    }

    #[derive(Deserialize)]
    struct StoreWelcomeQuery {
        user_id: i64,
        guild_id: i64,
        channel_id: i64,
        target_user_id: i64,
    }

    async fn onboarding_store_welcome(
        State(state): State<OnboardingServerState>,
        Query(q): Query<StoreWelcomeQuery>,
        body: axum::body::Bytes,
    ) -> StatusCode {
        state
            .add_member_posts
            .lock()
            .await
            .push((q.guild_id, q.channel_id, q.target_user_id));
        if q.user_id == 7 {
            let encoded = STANDARD.encode(body);
            *state.pending_welcome_b64.lock().await = Some(encoded);
        }
        StatusCode::NO_CONTENT
    }

    #[derive(Deserialize)]
    struct WelcomeQuery {
        user_id: i64,
    }

    async fn onboarding_fetch_welcome(
        State(state): State<OnboardingServerState>,
        Query(q): Query<WelcomeQuery>,
    ) -> Result<Json<WelcomeResponse>, StatusCode> {
        if q.user_id != 42 {
            return Err(StatusCode::NOT_FOUND);
        }
        let mut guard = state.pending_welcome_b64.lock().await;
        let Some(welcome_b64) = guard.take() else {
            return Err(StatusCode::NOT_FOUND);
        };
        Ok(Json(WelcomeResponse {
            guild_id: GuildId(11),
            channel_id: ChannelId(13),
            user_id: shared::domain::UserId(42),
            welcome_b64,
        }))
    }

    async fn onboarding_send_message(
        State(state): State<OnboardingServerState>,
        Json(payload): Json<SendMessageHttpRequest>,
    ) -> StatusCode {
        state
            .stored_ciphertexts
            .lock()
            .await
            .push(payload.ciphertext_b64);
        StatusCode::NO_CONTENT
    }

    async fn onboarding_messages_for_channel(
        State(state): State<OnboardingServerState>,
    ) -> Json<Vec<MessagePayload>> {
        let ciphertext_b64 = state
            .stored_ciphertexts
            .lock()
            .await
            .first()
            .cloned()
            .unwrap_or_default();
        Json(vec![MessagePayload {
            message_id: MessageId(1),
            channel_id: ChannelId(13),
            sender_id: shared::domain::UserId(7),
            sender_username: Some("adder".to_string()),
            ciphertext_b64,
            attachment: None,
            sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
        }])
    }

    async fn spawn_onboarding_server() -> Result<(String, OnboardingServerState)> {
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = OnboardingServerState {
            pending_welcome_b64: Arc::new(Mutex::new(None)),
            add_member_posts: Arc::new(Mutex::new(Vec::new())),
            stored_ciphertexts: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route(
                "/guilds/11/members",
                axum::routing::get(onboarding_list_members),
            )
            .route(
                "/mls/key_packages",
                axum::routing::get(onboarding_fetch_key_package),
            )
            .route(
                "/mls/welcome",
                axum::routing::post(onboarding_store_welcome),
            )
            .route("/mls/welcome", axum::routing::get(onboarding_fetch_welcome))
            .route(
                "/channels/13/messages",
                axum::routing::get(onboarding_messages_for_channel),
            )
            .route("/messages", axum::routing::post(onboarding_send_message))
            .with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{addr}"), state))
    }

    #[tokio::test]
    async fn added_member_retrieves_pending_welcome_and_auto_joins() {
        let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

        let adder_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
        let adder =
            RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(adder_mls));
        {
            let mut inner = adder.inner.lock().await;
            inner.server_url = Some(server_url.clone());
            inner.user_id = Some(7);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
            inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        }

        adder
            .select_channel(ChannelId(13))
            .await
            .expect("adder select");

        let posts = server_state.add_member_posts.lock().await.clone();
        assert_eq!(posts, vec![(11, 13, 42)]);

        let target_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
        let joined_welcomes = target_mls.joined_welcomes.clone();
        let target =
            RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
        {
            let mut inner = target.inner.lock().await;
            inner.server_url = Some(server_url);
            inner.user_id = Some(42);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
            inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        }

        target
            .select_channel(ChannelId(13))
            .await
            .expect("target select auto joins");

        let welcomes = joined_welcomes.lock().await.clone();
        assert_eq!(welcomes, vec![b"welcome-generated".to_vec()]);
    }

    #[tokio::test]
    async fn derive_livekit_e2ee_key_is_deterministic_for_same_connection() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::with_exported_secret(
                b"same-mls-secret-material".to_vec(),
            )),
        );

        let first = client
            .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
            .await
            .expect("first key");
        let second = client
            .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
            .await
            .expect("second key");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn derive_livekit_e2ee_key_is_domain_separated_by_channel() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::with_exported_secret(
                b"same-mls-secret-material".to_vec(),
            )),
        );

        let key_a = client
            .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
            .await
            .expect("key a");
        let key_b = client
            .derive_livekit_e2ee_key(GuildId(1), ChannelId(11))
            .await
            .expect("key b");

        assert_ne!(key_a, key_b);
    }

    #[tokio::test]
    async fn derive_livekit_e2ee_key_surfaces_missing_group_failure() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::failing("group not initialized")),
        );

        let err = client
            .derive_livekit_e2ee_key(GuildId(9), ChannelId(99))
            .await
            .expect_err("must fail");

        match err {
            LiveKitE2eeKeyError::MissingMlsGroup {
                guild_id,
                channel_id,
            } => {
                assert_eq!(guild_id, 9);
                assert_eq!(channel_id, 99);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn derive_livekit_e2ee_key_surfaces_export_failure() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::failing("backend export failed")),
        );

        let err = client
            .derive_livekit_e2ee_key(GuildId(9), ChannelId(100))
            .await
            .expect_err("must fail");

        match err {
            LiveKitE2eeKeyError::ExportFailure {
                guild_id,
                channel_id,
                source,
            } => {
                assert_eq!(guild_id, 9);
                assert_eq!(channel_id, 100);
                assert!(source.to_string().contains("backend export failed"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn derive_livekit_e2ee_key_rejects_invalid_cached_key_length() {
        let client = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::with_exported_secret(
                b"same-mls-secret-material".to_vec(),
            )),
        );

        {
            let mut inner = client.inner.lock().await;
            inner.voice_session_keys.insert(
                VoiceConnectionKey::new(GuildId(5), ChannelId(6)),
                CachedVoiceSessionKey {
                    key: vec![7u8; 8],
                    expires_at: Instant::now() + Duration::from_secs(30),
                },
            );
        }

        let err = client
            .derive_livekit_e2ee_key(GuildId(5), ChannelId(6))
            .await
            .expect_err("must fail");

        match err {
            LiveKitE2eeKeyError::InvalidDerivedKeyLength { expected, actual } => {
                assert_eq!(expected, LIVEKIT_E2EE_KEY_LEN);
                assert_eq!(actual, 8);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn onboarding_flow_encrypts_fetches_once_and_renders_plaintext() {
        let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

        let adder = RealtimeClient::new_with_mls_session_manager(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::ok(
                b"ciphertext-from-a".to_vec(),
                Vec::new(),
            )),
        );
        {
            let mut inner = adder.inner.lock().await;
            inner.server_url = Some(server_url.clone());
            inner.user_id = Some(7);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
            inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        }

        adder
            .select_channel(ChannelId(13))
            .await
            .expect("adder select");
        adder.send_message("hello from A").await.expect("send text");

        let stored_ciphertexts = server_state.stored_ciphertexts.lock().await.clone();
        assert_eq!(stored_ciphertexts.len(), 1);
        assert_ne!(
            stored_ciphertexts[0],
            STANDARD.encode("hello from A".as_bytes())
        );

        let target_mls = TestMlsSessionManager::ok(Vec::new(), b"hello from A".to_vec());
        let joined_welcomes = target_mls.joined_welcomes.clone();
        let decrypted_ciphertexts = target_mls.decrypted_ciphertexts.clone();
        let target =
            RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
        {
            let mut inner = target.inner.lock().await;
            inner.server_url = Some(server_url.clone());
            inner.user_id = Some(42);
            inner.selected_guild = Some(GuildId(11));
            inner.selected_channel = Some(ChannelId(13));
            inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        }

        let mut rx = target.subscribe_events();
        target
            .select_channel(ChannelId(13))
            .await
            .expect("target select auto joins");

        loop {
            let event = rx.recv().await.expect("decrypted event");
            if let ClientEvent::MessageDecrypted { plaintext, .. } = event {
                assert_eq!(plaintext, "hello from A");
                break;
            }
        }

        let welcomes = joined_welcomes.lock().await.clone();
        assert_eq!(welcomes, vec![b"welcome-generated".to_vec()]);

        let decrypt_inputs = decrypted_ciphertexts.lock().await.clone();
        assert_eq!(decrypt_inputs, vec![b"ciphertext-from-a".to_vec()]);

        let second_welcome_fetch = reqwest::Client::new()
            .get(format!(
                "{server_url}/mls/welcome?user_id=42&guild_id=11&channel_id=13"
            ))
            .send()
            .await
            .expect("second fetch request");
        assert_eq!(second_welcome_fetch.status(), StatusCode::NOT_FOUND);
    }

    struct MockLiveKitControlPlane {
        event: ServerEvent,
    }

    #[async_trait]
    impl LiveKitControlPlane for MockLiveKitControlPlane {
        async fn request_livekit_token(&self, request: ClientRequest) -> Result<ServerEvent> {
            match request {
                ClientRequest::RequestLiveKitToken { .. } => Ok(self.event.clone()),
                _ => Err(anyhow!("unexpected request")),
            }
        }
    }

    struct MockRoom {
        supports_screen: bool,
        e2ee_enabled: bool,
        events_tx: broadcast::Sender<LiveKitRoomEvent>,
        published: Arc<Mutex<Vec<LocalTrack>>>,
        unpublish_calls: Arc<Mutex<u32>>,
        leave_calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl LiveKitRoomSession for MockRoom {
        async fn publish_local_track(&self, track: LocalTrack) -> Result<()> {
            self.published.lock().await.push(track);
            Ok(())
        }

        async fn unpublish_local_tracks(&self) -> Result<()> {
            *self.unpublish_calls.lock().await += 1;
            Ok(())
        }

        async fn leave(&self) -> Result<()> {
            *self.leave_calls.lock().await += 1;
            Ok(())
        }

        fn supports_screen_share(&self) -> bool {
            self.supports_screen
        }

        fn is_e2ee_enabled(&self) -> bool {
            self.e2ee_enabled
        }

        fn subscribe_events(&self) -> broadcast::Receiver<LiveKitRoomEvent> {
            self.events_tx.subscribe()
        }
    }

    struct MockLiveKitConnector {
        room: Arc<MockRoom>,
        options_seen: Arc<Mutex<Vec<LiveKitRoomOptions>>>,
    }

    #[async_trait]
    impl LiveKitConnectorProvider for MockLiveKitConnector {
        async fn connect_room(
            &self,
            options: LiveKitRoomOptions,
        ) -> Result<Arc<dyn LiveKitRoomSession>> {
            self.options_seen.lock().await.push(options);
            Ok(self.room.clone())
        }
    }

    #[tokio::test]
    async fn connect_and_disconnect_voice_session_flow() {
        let room = Arc::new(MockRoom {
            supports_screen: true,
            e2ee_enabled: true,
            events_tx: broadcast::channel(32).0,
            published: Arc::new(Mutex::new(Vec::new())),
            unpublish_calls: Arc::new(Mutex::new(0)),
            leave_calls: Arc::new(Mutex::new(0)),
        });
        let options_seen = Arc::new(Mutex::new(Vec::new()));
        let client = RealtimeClient::new_with_dependencies(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::with_exported_secret(
                b"voice-secret-material".to_vec(),
            )),
            Arc::new(MockLiveKitControlPlane {
                event: ServerEvent::LiveKitTokenIssued {
                    guild_id: GuildId(11),
                    channel_id: ChannelId(13),
                    room_name: "g:11:c:13".to_string(),
                    token: "token-abc".to_string(),
                },
            }),
            Arc::new(MockLiveKitConnector {
                room: room.clone(),
                options_seen: options_seen.clone(),
            }),
        );

        client
            .connect_voice_session(VoiceConnectOptions {
                guild_id: GuildId(11),
                channel_id: ChannelId(13),
                can_publish_mic: true,
                can_publish_screen: true,
            })
            .await
            .expect("connect");

        assert_eq!(room.published.lock().await.len(), 2);
        let seen = options_seen.lock().await;
        assert_eq!(seen.len(), 1);
        assert!(seen[0].e2ee_enabled);
        assert_eq!(seen[0].room_name, "g:11:c:13");

        client.disconnect_voice_session().await.expect("disconnect");
        assert_eq!(*room.unpublish_calls.lock().await, 1);
        assert_eq!(*room.leave_calls.lock().await, 1);
    }

    #[tokio::test]
    async fn voice_participant_events_are_emitted() {
        let room = Arc::new(MockRoom {
            supports_screen: false,
            e2ee_enabled: true,
            events_tx: broadcast::channel(32).0,
            published: Arc::new(Mutex::new(Vec::new())),
            unpublish_calls: Arc::new(Mutex::new(0)),
            leave_calls: Arc::new(Mutex::new(0)),
        });

        let client = RealtimeClient::new_with_dependencies(
            PassthroughCrypto,
            Arc::new(TestMlsSessionManager::with_exported_secret(
                b"voice-secret-material".to_vec(),
            )),
            Arc::new(MockLiveKitControlPlane {
                event: ServerEvent::LiveKitTokenIssued {
                    guild_id: GuildId(1),
                    channel_id: ChannelId(2),
                    room_name: "g:1:c:2".to_string(),
                    token: "token".to_string(),
                },
            }),
            Arc::new(MockLiveKitConnector {
                room: room.clone(),
                options_seen: Arc::new(Mutex::new(Vec::new())),
            }),
        );

        let mut rx = client.subscribe_events();
        client
            .connect_voice_session(VoiceConnectOptions {
                guild_id: GuildId(1),
                channel_id: ChannelId(2),
                can_publish_mic: false,
                can_publish_screen: false,
            })
            .await
            .expect("connect");

        let _ = room.events_tx.send(LiveKitRoomEvent::ParticipantJoined(
            livekit_integration::RemoteParticipant {
                participant_id: "p1".to_string(),
                identity: "alice".to_string(),
            },
        ));
        let _ = room.events_tx.send(LiveKitRoomEvent::ParticipantLeft {
            participant_id: "p1".to_string(),
        });

        let mut got_join = false;
        let mut got_leave = false;
        for _ in 0..6 {
            if let Ok(ClientEvent::VoiceParticipantsUpdated { participants, .. }) = rx.recv().await
            {
                if participants.iter().any(|p| p.participant_id == "p1") {
                    got_join = true;
                }
                if participants.is_empty() {
                    got_leave = true;
                }
                if got_join && got_leave {
                    break;
                }
            }
        }

        assert!(got_join && got_leave);
    }
}
