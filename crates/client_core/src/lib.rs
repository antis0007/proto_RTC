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

pub mod error;
mod mls_session_manager;
pub mod protocol_client;
pub mod transport;
pub mod types;
pub use mls_session_manager::DurableMlsSessionManager;

const LIVEKIT_E2EE_EXPORT_LABEL: &str = "livekit-e2ee";
const LIVEKIT_E2EE_KEY_LEN: usize = 32;
const LIVEKIT_E2EE_CACHE_TTL: Duration = Duration::from_secs(90);
const LIVEKIT_E2EE_INFO_PREFIX: &[u8] = b"proto-rtc/livekit-e2ee/v1";
/// Deterministic application salt for LiveKit E2EE key derivation.
const LIVEKIT_E2EE_APP_SALT: &[u8] = b"proto-rtc/livekit-e2ee-app-salt";
const WELCOME_SYNC_RETRY_ATTEMPTS: usize = 12;
const WELCOME_SYNC_RETRY_DELAY: Duration = Duration::from_millis(500);
const WELCOME_SYNC_COOLDOWN_AFTER_EXHAUSTED: Duration = Duration::from_secs(15);
const BOOTSTRAP_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(30);

fn is_duplicate_member_add_error(err: &anyhow::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("duplicate signature key")
}

fn is_wrong_epoch_error(err: &anyhow::Error) -> bool {
    err.to_string().to_ascii_lowercase().contains("wrong epoch")
}

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
    async fn key_package_bytes(&self, guild_id: GuildId) -> Result<Vec<u8>>;
    async fn has_persisted_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool>;
    async fn open_or_create_group(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()>;
    async fn encrypt_application(&self, channel_id: ChannelId, plaintext: &[u8])
        -> Result<Vec<u8>>;
    async fn decrypt_application(
        &self,
        channel_id: ChannelId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>>;
    async fn add_member(
        &self,
        channel_id: ChannelId,
        key_package_bytes: &[u8],
    ) -> Result<MlsAddMemberOutcome>;
    async fn group_contains_key_package_identity(
        &self,
        channel_id: ChannelId,
        key_package_bytes: &[u8],
    ) -> Result<bool>;
    async fn join_from_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        welcome_bytes: &[u8],
    ) -> Result<()>;
    async fn export_secret(
        &self,
        channel_id: ChannelId,
        label: &str,
        len: usize,
    ) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MlsAddMemberOutcome {
    pub commit_bytes: Vec<u8>,
    pub welcome_bytes: Vec<u8>,
}

pub struct MissingMlsSessionManager;

#[async_trait]
impl MlsSessionManager for MissingMlsSessionManager {
    async fn key_package_bytes(&self, guild_id: GuildId) -> Result<Vec<u8>> {
        Err(anyhow!("MLS backend unavailable for guild {}", guild_id.0))
    }

    async fn has_persisted_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        Err(anyhow!(
            "MLS backend unavailable for guild {} channel {}",
            guild_id.0,
            channel_id.0
        ))
    }

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
    ) -> Result<MlsAddMemberOutcome> {
        Err(anyhow!(
            "no active MLS group available for channel {}",
            channel_id.0
        ))
    }

    async fn join_from_welcome(
        &self,
        _guild_id: GuildId,
        channel_id: ChannelId,
        _welcome_bytes: &[u8],
    ) -> Result<()> {
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

    async fn group_contains_key_package_identity(
        &self,
        channel_id: ChannelId,
        _key_package_bytes: &[u8],
    ) -> Result<bool> {
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
    initialized_mls_channels: HashSet<(GuildId, ChannelId)>,
    inflight_welcome_syncs: HashSet<(GuildId, ChannelId)>,
    welcome_sync_retry_after: HashMap<(GuildId, ChannelId), Instant>,
    bootstrap_request_last_sent: HashMap<(GuildId, ChannelId), Instant>,
    pending_outbound_plaintexts: HashMap<String, String>,
    voice_session_keys: HashMap<VoiceConnectionKey, CachedVoiceSessionKey>,
}

#[derive(Serialize)]
struct ListMessagesQuery {
    user_id: i64,
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
                initialized_mls_channels: HashSet::new(),
                inflight_welcome_syncs: HashSet::new(),
                welcome_sync_retry_after: HashMap::new(),
                bootstrap_request_last_sent: HashMap::new(),
                pending_outbound_plaintexts: HashMap::new(),
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
                            } else if let ServerEvent::GuildMembersUpdated { guild_id, members } =
                                &event
                            {
                                let _ = client.events.send(ClientEvent::Server(
                                    ServerEvent::GuildMembersUpdated {
                                        guild_id: *guild_id,
                                        members: members.clone(),
                                    },
                                ));
                                let guild_id = *guild_id;
                                {
                                    let mut guard = client.inner.lock().await;
                                    guard.welcome_sync_retry_after.retain(
                                        |(mapped_guild_id, _), _| *mapped_guild_id != guild_id,
                                    );
                                }
                                let client_clone = Arc::clone(&client);
                                tokio::spawn(async move {
                                    if let Err(err) =
                                        client_clone.reconcile_mls_state_for_guild(guild_id).await
                                    {
                                        let _ = client_clone.events.send(ClientEvent::Error(format!(
                                            "failed to reconcile MLS state for guild {} after membership update: {err}",
                                            guild_id.0
                                        )));
                                    }
                                });
                            } else if let ServerEvent::MlsWelcomeAvailable {
                                guild_id,
                                channel_id,
                                target_user_id,
                            } = event
                            {
                                let current_user_id = { client.inner.lock().await.user_id };
                                if current_user_id != Some(target_user_id.0) {
                                    continue;
                                }
                                {
                                    let mut guard = client.inner.lock().await;
                                    guard.channel_guilds.insert(channel_id, guild_id);
                                }
                                client.mark_welcome_sync_dirty(guild_id, channel_id).await;
                                let client_clone = Arc::clone(&client);
                                tokio::spawn(async move {
                                    if let Err(err) = client_clone
                                        .maybe_join_from_pending_welcome_with_retry(
                                            guild_id, channel_id,
                                        )
                                        .await
                                    {
                                        let _ = client_clone.events.send(ClientEvent::Error(
                                            format!(
                                                "failed MLS welcome sync for guild {} channel {}: {err}",
                                                guild_id.0, channel_id.0
                                            ),
                                        ));
                                    }
                                });
                            } else if let ServerEvent::MlsBootstrapRequested {
                                guild_id,
                                channel_id,
                                requesting_user_id,
                            } = event
                            {
                                client.mark_welcome_sync_dirty(guild_id, channel_id).await;
                                let client_clone = Arc::clone(&client);
                                tokio::spawn(async move {
                                    let Ok((_, current_user_id)) = client_clone.session().await
                                    else {
                                        return;
                                    };
                                    info!(
                                        guild_id = guild_id.0,
                                        channel_id = channel_id.0,
                                        requesting_user_id = requesting_user_id.0,
                                        current_user_id,
                                        "mls: received bootstrap request event"
                                    );
                                    if requesting_user_id.0 == current_user_id {
                                        info!(
                                            guild_id = guild_id.0,
                                            channel_id = channel_id.0,
                                            "mls: ignoring self-originated bootstrap request event"
                                        );
                                        return;
                                    }
                                    let _ = client_clone
                                        .maybe_bootstrap_existing_members_if_leader(
                                            guild_id,
                                            channel_id,
                                            current_user_id,
                                        )
                                        .await;
                                });
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
        let key_package_bytes = self.mls_session_manager.key_package_bytes(guild_id).await?;

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
        info!(
            guild_id = guild_id.0,
            channel_id = channel_id.0,
            target_user_id,
            actor_user_id = current_user_id,
            "mls: storing pending welcome"
        );
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
        info!(
            guild_id = guild_id.0,
            channel_id = channel_id.0,
            target_user_id,
            "mls: pending welcome stored"
        );
        Ok(())
    }

    async fn request_mls_bootstrap(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
        {
            let mut guard = self.inner.lock().await;
            if let Some(last_sent_at) = guard
                .bootstrap_request_last_sent
                .get(&(guild_id, channel_id))
                .copied()
            {
                if last_sent_at.elapsed() < BOOTSTRAP_REQUEST_MIN_INTERVAL {
                    info!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        "mls: bootstrap request suppressed due to rate limit"
                    );
                    return Ok(());
                }
            }
            guard
                .bootstrap_request_last_sent
                .insert((guild_id, channel_id), Instant::now());
        }
        let (server_url, user_id) = self.session().await?;
        self.http
            .post(format!("{server_url}/mls/bootstrap/request"))
            .query(&[
                ("user_id", user_id),
                ("guild_id", guild_id.0),
                ("channel_id", channel_id.0),
            ])
            .send()
            .await?
            .error_for_status()?;
        info!(
            guild_id = guild_id.0,
            channel_id = channel_id.0,
            requester_user_id = user_id,
            "mls: bootstrap request posted"
        );
        Ok(())
    }

    async fn maybe_join_from_pending_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
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
            info!(
                guild_id = guild_id.0,
                channel_id = channel_id.0,
                "mls: pending welcome not found"
            );
            return Ok(false);
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
            .join_from_welcome(guild_id, channel_id, &welcome_bytes)
            .await?;
        self.inner
            .lock()
            .await
            .initialized_mls_channels
            .insert((guild_id, channel_id));
        self.mark_welcome_sync_dirty(guild_id, channel_id).await;
        info!(
            guild_id = guild_id.0,
            channel_id = channel_id.0,
            "mls: welcome consumed and channel initialized"
        );
        Ok(true)
    }

    async fn mark_welcome_sync_dirty(&self, guild_id: GuildId, channel_id: ChannelId) {
        self.inner
            .lock()
            .await
            .welcome_sync_retry_after
            .remove(&(guild_id, channel_id));
    }

    async fn maybe_join_from_pending_welcome_with_retry(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        {
            let mut guard = self.inner.lock().await;
            if guard
                .inflight_welcome_syncs
                .contains(&(guild_id, channel_id))
            {
                info!(
                    guild_id = guild_id.0,
                    channel_id = channel_id.0,
                    "mls: welcome sync already in progress; skipping duplicate trigger"
                );
                return Ok(false);
            }
            if let Some(retry_after) = guard.welcome_sync_retry_after.get(&(guild_id, channel_id)) {
                if *retry_after > Instant::now() {
                    info!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        "mls: welcome sync skipped due to cooldown"
                    );
                    return Ok(false);
                }
            }
            guard.inflight_welcome_syncs.insert((guild_id, channel_id));
        }

        let result = async {
            let mut synced = false;
            for attempt in 0..WELCOME_SYNC_RETRY_ATTEMPTS {
                info!(
                    guild_id = guild_id.0,
                    channel_id = channel_id.0,
                    attempt = attempt + 1,
                    max_attempts = WELCOME_SYNC_RETRY_ATTEMPTS,
                    "mls: welcome sync attempt"
                );
                if self
                    .maybe_join_from_pending_welcome(guild_id, channel_id)
                    .await?
                {
                    info!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        attempt = attempt + 1,
                        "mls: welcome sync succeeded"
                    );
                    synced = true;
                    break;
                }

                if attempt + 1 < WELCOME_SYNC_RETRY_ATTEMPTS {
                    tokio::time::sleep(WELCOME_SYNC_RETRY_DELAY).await;
                }
            }

            if !synced {
                {
                    self.inner.lock().await.welcome_sync_retry_after.insert(
                        (guild_id, channel_id),
                        Instant::now() + WELCOME_SYNC_COOLDOWN_AFTER_EXHAUSTED,
                    );
                }
                let _ = self.request_mls_bootstrap(guild_id, channel_id).await;
            } else {
                self.mark_welcome_sync_dirty(guild_id, channel_id).await;
            }

            Ok(synced)
        }
        .await;

        self.inner
            .lock()
            .await
            .inflight_welcome_syncs
            .remove(&(guild_id, channel_id));

        result
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
        self.inner
            .lock()
            .await
            .initialized_mls_channels
            .insert((guild_id, channel_id));
        let members = self.fetch_members_for_guild(guild_id).await?;

        for member in members {
            if member.user_id.0 == current_user_id {
                continue;
            }

            let already_added = {
                let guard = self.inner.lock().await;
                guard.attempted_channel_member_additions.contains(&(
                    guild_id,
                    channel_id,
                    member.user_id.0,
                ))
            };
            if already_added {
                continue;
            }

            let key_package_bytes = match self.fetch_key_package(member.user_id.0, guild_id).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        target_user_id = member.user_id.0,
                        "mls: key package fetch failed during bootstrap: {err}"
                    );
                    continue;
                }
            };
            let is_already_in_group = match self
                .mls_session_manager
                .group_contains_key_package_identity(channel_id, &key_package_bytes)
                .await
            {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        target_user_id = member.user_id.0,
                        "mls: failed to inspect MLS roster before add_member: {err}"
                    );
                    false
                }
            };
            if is_already_in_group {
                info!(
                    guild_id = guild_id.0,
                    channel_id = channel_id.0,
                    target_user_id = member.user_id.0,
                    "mls: add_member skipped because target already appears in MLS roster"
                );
                self.inner
                    .lock()
                    .await
                    .attempted_channel_member_additions
                    .insert((guild_id, channel_id, member.user_id.0));
                continue;
            }

            let add_member_outcome = match self
                .mls_session_manager
                .add_member(channel_id, &key_package_bytes)
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => {
                    if is_duplicate_member_add_error(&err) {
                        info!(
                            guild_id = guild_id.0,
                            channel_id = channel_id.0,
                            target_user_id = member.user_id.0,
                            "mls: add_member skipped because target already appears in group"
                        );
                        self.inner
                            .lock()
                            .await
                            .attempted_channel_member_additions
                            .insert((guild_id, channel_id, member.user_id.0));
                        continue;
                    }
                    warn!(
                        guild_id = guild_id.0,
                        channel_id = channel_id.0,
                        target_user_id = member.user_id.0,
                        "mls: add_member failed during bootstrap: {err}"
                    );
                    continue;
                }
            };
            info!(
                guild_id = guild_id.0,
                channel_id = channel_id.0,
                target_user_id = member.user_id.0,
                "mls: add_member produced commit+welcome"
            );

            let commit_bytes_b64 = STANDARD.encode(&add_member_outcome.commit_bytes);
            if let Err(err) = self
                .post_ciphertext_message(guild_id, channel_id, commit_bytes_b64, None)
                .await
            {
                let message = format!(
                    "failed to post MLS add-member commit for user {} in guild {} channel {}: {err}",
                    member.user_id.0, guild_id.0, channel_id.0
                );
                let _ = self.events.send(ClientEvent::Error(message));
                continue;
            }

            if let Err(err) = self
                .store_pending_welcome(
                    guild_id,
                    channel_id,
                    member.user_id.0,
                    &add_member_outcome.welcome_bytes,
                )
                .await
            {
                let message = format!(
                    "failed to store pending MLS welcome for user {} in guild {} channel {}: {err}",
                    member.user_id.0, guild_id.0, channel_id.0
                );
                let _ = self.events.send(ClientEvent::Error(message));
                continue;
            }

            self.inner
                .lock()
                .await
                .attempted_channel_member_additions
                .insert((guild_id, channel_id, member.user_id.0));
        }

        Ok(())
    }

    async fn maybe_bootstrap_existing_members_if_leader(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        current_user_id: i64,
    ) -> Result<bool> {
        let members = match self.fetch_members_for_guild(guild_id).await {
            Ok(members) => members,
            Err(_) => return Ok(false),
        };
        let is_leader = members
            .iter()
            .map(|member| member.user_id.0)
            .min()
            .is_some_and(|leader_user_id| leader_user_id == current_user_id);
        if is_leader {
            self.maybe_add_existing_members_to_channel_group(guild_id, channel_id)
                .await?;
            return Ok(true);
        }
        Ok(false)
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
        let (guild_id, user_id, pending_plaintext) = {
            let mut guard = self.inner.lock().await;
            let user_id = guard
                .user_id
                .ok_or_else(|| anyhow!("not logged in: missing user_id"))?;
            if let Some(guild_id) = guard.channel_guilds.get(&message.channel_id).copied() {
                let pending_plaintext = if message.sender_id.0 == user_id {
                    guard
                        .pending_outbound_plaintexts
                        .remove(&message.ciphertext_b64)
                } else {
                    None
                };
                (guild_id, user_id, pending_plaintext)
            } else if guard.selected_channel == Some(message.channel_id) {
                if let Some(guild_id) = guard.selected_guild {
                    guard.channel_guilds.insert(message.channel_id, guild_id);
                    let pending_plaintext = if message.sender_id.0 == user_id {
                        guard
                            .pending_outbound_plaintexts
                            .remove(&message.ciphertext_b64)
                    } else {
                        None
                    };
                    (guild_id, user_id, pending_plaintext)
                } else {
                    return Err(anyhow!(
                        "missing guild mapping for channel {}",
                        message.channel_id.0
                    ));
                }
            } else {
                return Ok(());
            }
        };

        if message.sender_id.0 == user_id {
            if let Some(plaintext) = pending_plaintext {
                let _ = self.events.send(ClientEvent::MessageDecrypted {
                    message: message.clone(),
                    plaintext,
                });
            }
            return Ok(());
        }

        if !self
            .is_mls_channel_initialized(guild_id, message.channel_id)
            .await
        {
            let _ = self
                .maybe_join_from_pending_welcome_with_retry(guild_id, message.channel_id)
                .await;
            if !self
                .is_mls_channel_initialized(guild_id, message.channel_id)
                .await
            {
                return Ok(());
            }
        }
        let ciphertext = STANDARD
            .decode(message.ciphertext_b64.as_bytes())
            .with_context(|| {
                format!(
                    "invalid base64 ciphertext for message {}",
                    message.message_id.0
                )
            })?;

        let plaintext_bytes = match self
            .mls_session_manager
            .decrypt_application(message.channel_id, &ciphertext)
            .await
        {
            Ok(bytes) => bytes,
            Err(err) if is_wrong_epoch_error(&err) => {
                warn!(
                    guild_id = guild_id.0,
                    channel_id = message.channel_id.0,
                    message_id = message.message_id.0,
                    "mls: wrong epoch while decrypting; dropping local initialization and retrying welcome sync"
                );
                self.inner
                    .lock()
                    .await
                    .initialized_mls_channels
                    .remove(&(guild_id, message.channel_id));
                let _ = self
                    .maybe_join_from_pending_welcome_with_retry(guild_id, message.channel_id)
                    .await;
                return Ok(());
            }
            Err(err) => return Err(err),
        };

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
        let (_server_url, user_id, guild_id, channel_id) = self.active_context().await?;
        if let Err(err) = self
            .maybe_bootstrap_existing_members_if_leader(guild_id, channel_id, user_id)
            .await
        {
            let _ = self.events.send(ClientEvent::Error(format!(
                "failed to bootstrap MLS membership for guild {} channel {}: {err}",
                guild_id.0, channel_id.0
            )));
        }

        if !self
            .ensure_mls_channel_initialized(guild_id, channel_id)
            .await?
        {
            let _ = self
                .maybe_bootstrap_existing_members_if_leader(guild_id, channel_id, user_id)
                .await?;
            let _ = self.reconcile_mls_state_for_guild(guild_id).await;

            if !self
                .ensure_mls_channel_initialized(guild_id, channel_id)
                .await?
            {
                return Err(anyhow!(
                    "MLS state for guild {} channel {} is uninitialized; wait for welcome sync before sending",
                    guild_id.0,
                    channel_id.0
                ));
            }
        }

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

        {
            let mut guard = self.inner.lock().await;
            guard
                .pending_outbound_plaintexts
                .insert(payload.ciphertext_b64.clone(), text.to_string());
        }

        if let Err(err) = self.post_send_message_payload(payload.clone()).await {
            self.inner
                .lock()
                .await
                .pending_outbound_plaintexts
                .remove(&payload.ciphertext_b64);
            return Err(err);
        }

        let websocket_active = { self.inner.lock().await.ws_started };
        if !websocket_active {
            if let Err(err) = self.fetch_messages_impl(channel_id, 1, None).await {
                let _ = self.events.send(ClientEvent::Error(format!(
                    "message sent but local refresh failed without websocket: {err}"
                )));
            }
        }

        Ok(())
    }

    async fn post_ciphertext_message(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        ciphertext_b64: String,
        attachment: Option<AttachmentPayload>,
    ) -> Result<()> {
        let (_server_url, user_id) = self.session().await?;
        let payload = SendMessageHttpRequest {
            user_id,
            guild_id: guild_id.0,
            channel_id: channel_id.0,
            ciphertext_b64,
            attachment,
        };
        self.post_send_message_payload(payload).await
    }

    async fn ensure_mls_channel_initialized(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        if self.is_mls_channel_initialized(guild_id, channel_id).await {
            return Ok(true);
        }

        if self
            .try_restore_mls_channel_from_local_state(guild_id, channel_id)
            .await?
        {
            return Ok(true);
        }

        if self
            .maybe_join_from_pending_welcome_with_retry(guild_id, channel_id)
            .await?
        {
            return Ok(true);
        }

        Ok(false)
    }

    async fn try_restore_mls_channel_from_local_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        if !self
            .mls_session_manager
            .has_persisted_group_state(guild_id, channel_id)
            .await?
        {
            return Ok(false);
        }

        self.mls_session_manager
            .open_or_create_group(guild_id, channel_id)
            .await?;
        self.inner
            .lock()
            .await
            .initialized_mls_channels
            .insert((guild_id, channel_id));
        Ok(true)
    }

    async fn fetch_messages_impl(
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

    async fn is_mls_channel_initialized(&self, guild_id: GuildId, channel_id: ChannelId) -> bool {
        self.inner
            .lock()
            .await
            .initialized_mls_channels
            .contains(&(guild_id, channel_id))
    }

    async fn post_send_message_payload(&self, payload: SendMessageHttpRequest) -> Result<()> {
        let (server_url, _user_id) = self.session().await?;
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

impl<C: CryptoProvider + 'static> RealtimeClient<C> {
    async fn prewarm_mls_for_guild_channels(
        &self,
        guild_id: GuildId,
        channels: &[ChannelSummary],
    ) -> Result<()> {
        let (_, user_id) = self.session().await?;
        for channel in channels {
            if channel.kind != shared::domain::ChannelKind::Text {
                continue;
            }

            let channel_id = channel.channel_id;
            let _ = self
                .maybe_bootstrap_existing_members_if_leader(guild_id, channel_id, user_id)
                .await;
            let _ = self
                .maybe_join_from_pending_welcome_with_retry(guild_id, channel_id)
                .await;
            let _ = self
                .try_restore_mls_channel_from_local_state(guild_id, channel_id)
                .await;
        }

        Ok(())
    }

    async fn reconcile_mls_state_for_guild(&self, guild_id: GuildId) -> Result<()> {
        let (server_url, user_id) = self.session().await?;
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

        self.prewarm_mls_for_guild_channels(guild_id, &channels)
            .await
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
            guard.ws_started = false;
            guard.channel_guilds.clear();
            guard.sender_directory.clear();
            guard.pending_outbound_plaintexts.clear();
            guard.initialized_mls_channels.clear();
            guard.inflight_welcome_syncs.clear();
            guard.bootstrap_request_last_sent.clear();
            guard.attempted_channel_member_additions.clear();
            zeroize_voice_session_cache(&mut guard);
        }

        if let Err(err) = self.spawn_ws_events(server_url, body.user_id).await {
            let mut guard = self.inner.lock().await;
            guard.server_url = None;
            guard.user_id = None;
            guard.ws_started = false;
            guard.selected_guild = None;
            guard.selected_channel = None;
            guard.channel_guilds.clear();
            guard.sender_directory.clear();
            guard.pending_outbound_plaintexts.clear();
            guard.initialized_mls_channels.clear();
            guard.inflight_welcome_syncs.clear();
            guard.bootstrap_request_last_sent.clear();
            guard.attempted_channel_member_additions.clear();
            zeroize_voice_session_cache(&mut guard);
            return Err(err);
        }

        {
            let mut guard = self.inner.lock().await;
            guard.ws_started = true;
        }

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

        for channel in &channels {
            let _ = self
                .events
                .send(ClientEvent::Server(ServerEvent::ChannelUpdated {
                    channel: channel.clone(),
                }));
        }

        if let Err(err) = self
            .prewarm_mls_for_guild_channels(guild_id, &channels)
            .await
        {
            let _ = self.events.send(ClientEvent::Error(format!(
                "failed to prewarm MLS state for guild {}: {err}",
                guild_id.0
            )));
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

        let (_, current_user_id) = self.session().await?;
        let _ = self
            .maybe_bootstrap_existing_members_if_leader(guild_id, channel_id, current_user_id)
            .await?;

        let initialized = self
            .ensure_mls_channel_initialized(guild_id, channel_id)
            .await?;
        if !initialized {
            let client = Arc::clone(self);
            tokio::spawn(async move {
                let _ = client.reconcile_mls_state_for_guild(guild_id).await;
                let _ = client
                    .maybe_join_from_pending_welcome_with_retry(guild_id, channel_id)
                    .await;
            });
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
        self.fetch_messages_impl(channel_id, limit, before).await
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
            let _ = self.reconcile_mls_state_for_guild(guild_id).await;
            let client = Arc::clone(self);
            tokio::spawn(async move {
                for _ in 0..6 {
                    let _ = client.reconcile_mls_state_for_guild(guild_id).await;
                    tokio::time::sleep(Duration::from_millis(350)).await;
                }
            });
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
#[path = "tests/lib_tests.rs"]
mod tests;
