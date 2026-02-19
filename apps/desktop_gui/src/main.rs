use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    thread,
    time::SystemTime,
};

mod backend_bridge;
mod controller;
mod media;
mod ui;

use arboard::{Clipboard, ImageData};
use client_core::{
    AttachmentUpload, ClientEvent, ClientHandle, DurableMlsSessionManager, PassthroughCrypto,
    RealtimeClient, VoiceConnectOptions, VoiceParticipantState, VoiceSessionSnapshot,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use eframe::egui;
use egui::TextureHandle;
use image::GenericImageView;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId, Role},
    protocol::{
        AttachmentPayload, ChannelSummary, GuildSummary, MemberSummary, MessagePayload, ServerEvent,
    },
};

enum BackendCommand {
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

enum UiEvent {
    LoginOk,
    Info(String),
    InviteCreated(String),
    SenderDirectoryUpdated {
        user_id: i64,
        username: String,
    },
    Error(UiError),
    AttachmentPreviewLoaded {
        file_id: FileId,
        image: PreviewImage,
        original_bytes: Vec<u8>,
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
enum UiErrorCategory {
    Auth,
    Transport,
    Crypto,
    Validation,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiErrorContext {
    BackendStartup,
    Login,
    SendMessage,
    DecryptMessage,
    General,
}

fn classify_login_failure(message: &str) -> String {
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
struct UiError {
    category: UiErrorCategory,
    context: UiErrorContext,
    message: String,
}

impl UiError {
    fn from_message(context: UiErrorContext, message: impl Into<String>) -> Self {
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

    fn requires_reauth(&self) -> bool {
        self.category == UiErrorCategory::Auth
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusBannerSeverity {
    Error,
}

#[derive(Debug, Clone)]
struct StatusBanner {
    severity: StatusBannerSeverity,
    message: String,
}

fn err_label(category: UiErrorCategory) -> &'static str {
    match category {
        UiErrorCategory::Auth => "Authentication",
        UiErrorCategory::Transport => "Transport",
        UiErrorCategory::Crypto => "Crypto",
        UiErrorCategory::Validation => "Validation",
        UiErrorCategory::Unknown => "Unexpected",
    }
}

fn lighten_color(c: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |channel: u8| -> u8 {
        let channel = channel as f32;
        (channel + (255.0 - channel) * t).round().clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgba_unmultiplied(mix(c.r()), mix(c.g()), mix(c.b()), c.a())
}

fn server_environment_label(server_url: &str) -> &'static str {
    let server = server_url.to_ascii_lowercase();
    if server.contains("127.0.0.1") || server.contains("localhost") {
        "Local"
    } else if server.contains("staging") {
        "Staging"
    } else if server.contains("dev") {
        "Development"
    } else {
        "Production"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceSessionConnectionStatus {
    Idle,
    Connecting,
    Connected,
    Disconnecting,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountPresence {
    Online,
    Away,
    DoNotDisturb,
}

impl AccountPresence {
    fn label(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Away => "Away",
            Self::DoNotDisturb => "Do Not Disturb",
        }
    }
}

#[derive(Debug, Clone)]
struct VoiceSessionUiState {
    selected_voice_channel: Option<ChannelId>,
    active_session: Option<VoiceSessionSnapshot>,
    participants_by_channel: HashMap<ChannelId, Vec<VoiceParticipantState>>,
    connection_status: VoiceSessionConnectionStatus,
    muted: bool,
    deafened: bool,
    screen_share_enabled: bool,
    last_error: Option<String>,
}

impl VoiceSessionUiState {
    fn new() -> Self {
        Self {
            selected_voice_channel: None,
            active_session: None,
            participants_by_channel: HashMap::new(),
            connection_status: VoiceSessionConnectionStatus::Idle,
            muted: false,
            deafened: false,
            screen_share_enabled: false,
            last_error: None,
        }
    }

    fn occupancy_for_channel(&self, channel_id: ChannelId) -> usize {
        self.participants_by_channel
            .get(&channel_id)
            .map(|participants| participants.len())
            .unwrap_or(0)
    }

    fn active_participant_count(&self) -> usize {
        self.active_session
            .as_ref()
            .map(|snapshot| self.occupancy_for_channel(snapshot.channel_id))
            .unwrap_or(0)
    }

    fn is_channel_connected(&self, channel_id: ChannelId) -> bool {
        self.active_session
            .as_ref()
            .map(|session| session.channel_id == channel_id)
            .unwrap_or(false)
    }
}

#[derive(Clone)]
struct DisplayMessage {
    wire: MessagePayload,
    plaintext: String,
}

#[derive(Clone)]
struct PreviewImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

enum AttachmentPreviewState {
    NotRequested,
    Loading,
    Ready {
        image: PreviewImage,
        original_bytes: Vec<u8>,
        preview_png: Option<Vec<u8>>,
        texture: Option<egui::TextureHandle>,
    },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemePreset {
    DiscordDark,
    DiscordLegacy,
    AtomOneDark,
    EguiLight,
}

impl ThemePreset {
    fn label(self) -> &'static str {
        match self {
            ThemePreset::DiscordDark => "Discord (Dark)",
            ThemePreset::DiscordLegacy => "Discord (Legacy)",
            ThemePreset::AtomOneDark => "Atom One Dark",
            ThemePreset::EguiLight => "Egui Light",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ThemeSettings {
    preset: ThemePreset,
    accent_color: egui::Color32,
    panel_rounding: u8,
    list_row_shading: bool,
}

impl ThemeSettings {
    fn discord_default() -> Self {
        Self {
            preset: ThemePreset::DiscordDark,
            accent_color: egui::Color32::from_rgb(88, 101, 242),
            panel_rounding: 10,
            list_row_shading: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct UiReadabilitySettings {
    text_scale: f32,
    compact_density: bool,
    show_timestamps: bool,
    message_bubble_backgrounds: bool,
}

impl UiReadabilitySettings {
    fn defaults() -> Self {
        Self {
            text_scale: 1.0,
            compact_density: false,
            show_timestamps: true,
            message_bubble_backgrounds: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum PersistedThemePreset {
    DiscordDark,
    DiscordLegacy,
    AtomOneDark,
    EguiLight,
}

impl From<ThemePreset> for PersistedThemePreset {
    fn from(value: ThemePreset) -> Self {
        match value {
            ThemePreset::DiscordDark => Self::DiscordDark,
            ThemePreset::DiscordLegacy => Self::DiscordLegacy,
            ThemePreset::AtomOneDark => Self::AtomOneDark,
            ThemePreset::EguiLight => Self::EguiLight,
        }
    }
}

impl From<PersistedThemePreset> for ThemePreset {
    fn from(value: PersistedThemePreset) -> Self {
        match value {
            PersistedThemePreset::DiscordDark => Self::DiscordDark,
            PersistedThemePreset::DiscordLegacy => Self::DiscordLegacy,
            PersistedThemePreset::AtomOneDark => Self::AtomOneDark,
            PersistedThemePreset::EguiLight => Self::EguiLight,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PersistedDesktopSettings {
    theme_preset: PersistedThemePreset,
    accent_color: [u8; 4],
    panel_rounding: u8,
    list_row_shading: bool,
    text_scale: f32,
    compact_density: bool,
    show_timestamps: bool,
    message_bubble_backgrounds: bool,
    composer_panel_height: f32,
    left_user_panel_height: f32,
}

const DEFAULT_COMPOSER_PANEL_HEIGHT: f32 = 66.0;
const MIN_COMPOSER_PANEL_HEIGHT: f32 = 52.0;
const MAX_COMPOSER_PANEL_HEIGHT: f32 = 220.0;

const DEFAULT_LEFT_USER_PANEL_HEIGHT: f32 = DEFAULT_COMPOSER_PANEL_HEIGHT;
const MIN_LEFT_USER_PANEL_HEIGHT: f32 = 56.0;
const MAX_LEFT_USER_PANEL_HEIGHT: f32 = 220.0;

impl Default for PersistedDesktopSettings {
    fn default() -> Self {
        let theme = ThemeSettings::discord_default();
        let readability = UiReadabilitySettings::defaults();
        Self {
            theme_preset: theme.preset.into(),
            accent_color: [
                theme.accent_color.r(),
                theme.accent_color.g(),
                theme.accent_color.b(),
                theme.accent_color.a(),
            ],
            panel_rounding: theme.panel_rounding,
            list_row_shading: theme.list_row_shading,
            text_scale: readability.text_scale,
            compact_density: readability.compact_density,
            show_timestamps: readability.show_timestamps,
            message_bubble_backgrounds: readability.message_bubble_backgrounds,
            composer_panel_height: DEFAULT_COMPOSER_PANEL_HEIGHT,
            left_user_panel_height: DEFAULT_LEFT_USER_PANEL_HEIGHT,
        }
    }
}

impl PersistedDesktopSettings {
    fn into_runtime(self) -> (ThemeSettings, UiReadabilitySettings, f32, f32) {
        (
            ThemeSettings {
                preset: self.theme_preset.into(),
                accent_color: egui::Color32::from_rgba_unmultiplied(
                    self.accent_color[0],
                    self.accent_color[1],
                    self.accent_color[2],
                    self.accent_color[3],
                ),
                panel_rounding: self.panel_rounding.min(16),
                list_row_shading: self.list_row_shading,
            },
            UiReadabilitySettings {
                text_scale: self.text_scale.clamp(0.8, 1.4),
                compact_density: self.compact_density,
                show_timestamps: self.show_timestamps,
                message_bubble_backgrounds: self.message_bubble_backgrounds,
            },
            self.composer_panel_height
                .clamp(MIN_COMPOSER_PANEL_HEIGHT, MAX_COMPOSER_PANEL_HEIGHT),
            self.left_user_panel_height
                .clamp(MIN_LEFT_USER_PANEL_HEIGHT, MAX_LEFT_USER_PANEL_HEIGHT),
        )
    }

    fn from_runtime(
        theme: ThemeSettings,
        readability: UiReadabilitySettings,
        composer_panel_height: f32,
        left_user_panel_height: f32,
    ) -> Self {
        Self {
            theme_preset: theme.preset.into(),
            accent_color: [
                theme.accent_color.r(),
                theme.accent_color.g(),
                theme.accent_color.b(),
                theme.accent_color.a(),
            ],
            panel_rounding: theme.panel_rounding,
            list_row_shading: theme.list_row_shading,
            text_scale: readability.text_scale,
            compact_density: readability.compact_density,
            show_timestamps: readability.show_timestamps,
            message_bubble_backgrounds: readability.message_bubble_backgrounds,
            composer_panel_height: composer_panel_height
                .clamp(MIN_COMPOSER_PANEL_HEIGHT, MAX_COMPOSER_PANEL_HEIGHT),
            left_user_panel_height: left_user_panel_height
                .clamp(MIN_LEFT_USER_PANEL_HEIGHT, MAX_LEFT_USER_PANEL_HEIGHT),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppViewState {
    Login,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginFocusField {
    Server,
    Username,
}

#[derive(Debug, Clone)]
struct LoginUiState {
    focus: Option<LoginFocusField>,
    attempted_auto_focus: bool,
    last_login_click_tick: u64,
}

impl Default for LoginUiState {
    fn default() -> Self {
        Self {
            focus: Some(LoginFocusField::Username),
            attempted_auto_focus: false,
            last_login_click_tick: 0,
        }
    }
}

struct DesktopGuiApp {
    cmd_tx: Sender<BackendCommand>,
    ui_rx: Receiver<UiEvent>,

    server_url: String,
    username: String,
    invite_code_input: String,
    auth_session_established: bool,
    presence_preference: AccountPresence,
    notifications_enabled: bool,
    desktop_notifications_enabled: bool,
    mention_notifications_enabled: bool,
    display_name_draft: String,

    composer: String,
    pending_attachment: Option<PathBuf>,
    attachment_preview_cache: HashMap<AttachmentPreviewCacheKey, AttachmentPreview>,

    guilds: Vec<GuildSummary>,
    channels: Vec<ChannelSummary>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,

    messages: HashMap<ChannelId, Vec<DisplayMessage>>,
    members: HashMap<GuildId, Vec<MemberSummary>>,
    message_ids: HashMap<ChannelId, HashSet<MessageId>>,

    status: String,
    status_banner: Option<StatusBanner>,

    sender_directory: HashMap<i64, String>,
    attachment_previews: HashMap<FileId, AttachmentPreviewState>,
    expanded_preview: Option<FileId>,
    hovered_message: Option<MessageId>,

    voice_ui: VoiceSessionUiState,

    settings_open: bool,
    view_state: AppViewState,

    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
    readability: UiReadabilitySettings,
    applied_readability: Option<UiReadabilitySettings>,
    composer_panel_height: f32,
    left_user_panel_height: f32,

    // New: stable per-view UI state so text boxes keep focus reliably.
    login_ui: LoginUiState,

    // Simple frame tick (used for debouncing and UI heuristics).
    tick: u64,
}

#[derive(Clone)]
enum AttachmentPreview {
    Image {
        texture: TextureHandle,
        size: egui::Vec2,
        preview_png: Vec<u8>,
    },
    DecodeFailed,
}

#[derive(Clone, Eq)]
struct AttachmentPreviewCacheKey {
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl PartialEq for AttachmentPreviewCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.modified == other.modified
    }
}

impl Hash for AttachmentPreviewCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.modified.hash(state);
    }
}

impl DesktopGuiApp {
    fn new(
        cmd_tx: Sender<BackendCommand>,
        ui_rx: Receiver<UiEvent>,
        persisted_settings: Option<PersistedDesktopSettings>,
    ) -> Self {
        let (theme, readability, composer_panel_height, left_user_panel_height) =
            persisted_settings.unwrap_or_default().into_runtime();
        Self {
            cmd_tx,
            ui_rx,
            server_url: "http://127.0.0.1:8443".to_string(),
            username: "alice".to_string(),
            invite_code_input: String::new(),
            auth_session_established: false,
            presence_preference: AccountPresence::Online,
            notifications_enabled: true,
            desktop_notifications_enabled: true,
            mention_notifications_enabled: true,
            display_name_draft: "alice".to_string(),
            composer: String::new(),
            pending_attachment: None,
            attachment_preview_cache: HashMap::new(),
            guilds: Vec::new(),
            channels: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            messages: HashMap::new(),
            members: HashMap::new(),
            message_ids: HashMap::new(),
            status: "Not logged in".to_string(),
            status_banner: None,
            sender_directory: HashMap::new(),
            attachment_previews: HashMap::new(),
            expanded_preview: None,
            hovered_message: None,
            voice_ui: VoiceSessionUiState::new(),
            settings_open: false,
            view_state: AppViewState::Login,
            theme,
            applied_theme: None,
            readability,
            applied_readability: None,
            composer_panel_height,
            left_user_panel_height,
            login_ui: LoginUiState::default(),
            tick: 0,
        }
    }

    fn process_ui_events(&mut self) {
        while let Ok(event) = self.ui_rx.try_recv() {
            match event {
                UiEvent::LoginOk => {
                    self.auth_session_established = true;
                    self.view_state = AppViewState::Main;
                    self.status = "Logged in - syncing guilds".to_string();
                    self.status_banner = None;
                    self.guilds.clear();
                    self.channels.clear();
                    self.messages.clear();
                    self.members.clear();
                    self.message_ids.clear();
                    self.selected_guild = None;
                    self.selected_channel = None;
                    self.sender_directory.clear();
                    self.attachment_previews.clear();
                    self.expanded_preview = None;
                    self.voice_ui = VoiceSessionUiState::new();
                    queue_command(&self.cmd_tx, BackendCommand::ListGuilds, &mut self.status);
                }
                UiEvent::Info(message) => {
                    self.status = message;
                }
                UiEvent::InviteCreated(invite_code) => {
                    self.invite_code_input = invite_code;
                    if let Ok(mut clipboard) = Clipboard::new() {
                        let _ = clipboard.set_text(self.invite_code_input.clone());
                    }
                    self.status =
                        "Invite created, copied to clipboard, and inserted into the Invite field"
                            .to_string();
                }
                UiEvent::SenderDirectoryUpdated { user_id, username } => {
                    self.sender_directory.insert(user_id, username);
                }
                UiEvent::Error(err) => {
                    if err.requires_reauth() {
                        self.auth_session_established = false;
                        self.view_state = AppViewState::Login;
                        self.status = format!("Authentication error: {}", err.message);
                        self.status_banner = Some(StatusBanner {
                            severity: StatusBannerSeverity::Error,
                            message:
                                "Session expired or invalid credentials. Please sign in again."
                                    .to_string(),
                        });
                        self.login_ui.focus = Some(LoginFocusField::Username);
                    } else {
                        self.status = if err.context == UiErrorContext::Login {
                            classify_login_failure(&err.message)
                        } else {
                            format!("{} error: {}", err_label(err.category), err.message)
                        };
                        if matches!(
                            err.context,
                            UiErrorContext::Login
                                | UiErrorContext::SendMessage
                                | UiErrorContext::BackendStartup
                        ) {
                            self.status_banner = Some(StatusBanner {
                                severity: StatusBannerSeverity::Error,
                                message: self.status.clone(),
                            });
                        }
                    }
                }
                UiEvent::VoiceOperationFailed(err) => {
                    self.voice_ui.connection_status = VoiceSessionConnectionStatus::Error;
                    self.voice_ui.last_error = Some(err.clone());
                    self.status = format!("Voice error: {err}");
                }
                UiEvent::VoiceSessionStateChanged(snapshot) => match snapshot {
                    Some(active) => {
                        self.voice_ui.connection_status = VoiceSessionConnectionStatus::Connected;
                        self.voice_ui.selected_voice_channel = Some(active.channel_id);
                        self.voice_ui.active_session = Some(active);
                        self.voice_ui.last_error = None;
                    }
                    None => {
                        self.voice_ui.connection_status = VoiceSessionConnectionStatus::Idle;
                        self.voice_ui.active_session = None;
                        self.voice_ui.muted = false;
                        self.voice_ui.deafened = false;
                        self.voice_ui.screen_share_enabled = false;
                    }
                },
                UiEvent::VoiceParticipantsUpdated {
                    guild_id,
                    channel_id,
                    participants,
                } => {
                    if self.selected_guild == Some(guild_id) {
                        self.voice_ui
                            .participants_by_channel
                            .insert(channel_id, participants);
                    }
                }
                UiEvent::AttachmentPreviewLoaded {
                    file_id,
                    image,
                    original_bytes,
                } => {
                    let preview_png = encode_rgba_png(&image.rgba, image.width, image.height).ok();
                    self.attachment_previews.insert(
                        file_id,
                        AttachmentPreviewState::Ready {
                            image,
                            original_bytes,
                            preview_png,
                            texture: None,
                        },
                    );
                }
                UiEvent::AttachmentPreviewFailed { file_id, reason } => {
                    self.attachment_previews
                        .insert(file_id, AttachmentPreviewState::Error(reason));
                }
                UiEvent::MessageDecrypted { message, plaintext } => {
                    if let Some(username) = &message.sender_username {
                        self.sender_directory
                            .insert(message.sender_id.0, username.clone());
                    }

                    let ids = self.message_ids.entry(message.channel_id).or_default();
                    if ids.insert(message.message_id) {
                        let messages = self.messages.entry(message.channel_id).or_default();
                        messages.push(DisplayMessage {
                            wire: message,
                            plaintext,
                        });
                        messages.sort_by_key(|m| m.wire.message_id.0);
                    }
                }
                UiEvent::Server(server_event) => match server_event {
                    ServerEvent::GuildUpdated { guild } => {
                        let guild_id = guild.guild_id;
                        if !self.guilds.iter().any(|g| g.guild_id == guild_id) {
                            self.guilds.push(guild);
                        }
                        if self.selected_guild.is_none() {
                            self.selected_guild = Some(guild_id);
                            self.channels.clear();
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::ListChannels { guild_id },
                                &mut self.status,
                            );
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::ListMembers { guild_id },
                                &mut self.status,
                            );
                        }
                    }
                    ServerEvent::ChannelUpdated { channel } => {
                        if !self
                            .channels
                            .iter()
                            .any(|c| c.channel_id == channel.channel_id)
                        {
                            let channel_id = channel.channel_id;
                            self.channels.push(channel);
                            if self.selected_channel.is_none() {
                                self.selected_channel = Some(channel_id);
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::SelectChannel { channel_id },
                                    &mut self.status,
                                );
                            }
                        }
                    }
                    ServerEvent::GuildMembersUpdated { guild_id, members } => {
                        if self.selected_guild == Some(guild_id) {
                            self.members.insert(guild_id, members);
                        }
                    }
                    ServerEvent::Error(err) => {
                        self.status = format!("Server error: {}", err.message);
                    }
                    _ => {}
                },
            }
        }
    }

    fn oldest_message_id(&self, channel_id: ChannelId) -> Option<MessageId> {
        self.messages
            .get(&channel_id)
            .and_then(|messages| messages.first())
            .map(|message| message.wire.message_id)
    }

    fn selected_voice_channel(&self) -> Option<&ChannelSummary> {
        let channel_id = self.voice_ui.selected_voice_channel?;
        self.channels
            .iter()
            .find(|channel| channel.channel_id == channel_id && channel.kind == ChannelKind::Voice)
    }

    fn selected_text_channel_id(&self) -> Option<ChannelId> {
        let channel_id = self.selected_channel?;
        self.channels
            .iter()
            .find(|channel| channel.channel_id == channel_id && channel.kind == ChannelKind::Text)
            .map(|channel| channel.channel_id)
    }

    fn voice_status_badge(&self) -> (&'static str, egui::Color32) {
        match self.voice_ui.connection_status {
            VoiceSessionConnectionStatus::Idle => ("idle", egui::Color32::GRAY),
            VoiceSessionConnectionStatus::Connecting => ("connecting", egui::Color32::YELLOW),
            VoiceSessionConnectionStatus::Connected => ("connected", egui::Color32::GREEN),
            VoiceSessionConnectionStatus::Disconnecting => {
                ("disconnecting", egui::Color32::LIGHT_BLUE)
            }
            VoiceSessionConnectionStatus::Error => ("error", egui::Color32::RED),
        }
    }

    fn apply_theme_if_needed(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.theme)
            && self.applied_readability == Some(self.readability)
        {
            return;
        }

        let mut style = (*ctx.style()).clone();
        style.visuals = visuals_for_theme(self.theme);
        style.text_styles = scaled_text_styles(self.readability.text_scale);

        // Make text inputs reliably clickable and visible:
        style.visuals.widgets.inactive.bg_stroke =
            egui::Stroke::new(1.0, style.visuals.widgets.noninteractive.bg_stroke.color);
        style.visuals.widgets.hovered.bg_stroke =
            egui::Stroke::new(1.0, style.visuals.widgets.hovered.bg_stroke.color);
        style.visuals.widgets.active.bg_stroke =
            egui::Stroke::new(1.2, style.visuals.selection.bg_fill.gamma_multiply(0.9));

        if self.readability.compact_density {
            style.spacing.item_spacing = egui::vec2(6.0, 4.0);
            style.spacing.button_padding = egui::vec2(8.0, 5.0);
            style.spacing.interact_size = egui::vec2(40.0, 24.0);
        } else {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            style.spacing.interact_size = egui::vec2(40.0, 30.0);
        }
        ctx.set_style(style);
        self.applied_theme = Some(self.theme);
        self.applied_readability = Some(self.readability);
    }

    fn show_settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }

        egui::Window::new("Settings")
            .open(&mut self.settings_open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Theme preset");
                egui::ComboBox::from_id_source("theme_preset")
                    .selected_text(self.theme.preset.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.theme.preset,
                            ThemePreset::DiscordDark,
                            ThemePreset::DiscordDark.label(),
                        );
                        ui.selectable_value(
                            &mut self.theme.preset,
                            ThemePreset::DiscordLegacy,
                            ThemePreset::DiscordLegacy.label(),
                        );
                        ui.selectable_value(
                            &mut self.theme.preset,
                            ThemePreset::AtomOneDark,
                            ThemePreset::AtomOneDark.label(),
                        );
                        ui.selectable_value(
                            &mut self.theme.preset,
                            ThemePreset::EguiLight,
                            ThemePreset::EguiLight.label(),
                        );
                    });

                ui.separator();
                ui.label("Accent color");
                ui.color_edit_button_srgba(&mut self.theme.accent_color);
                ui.small("Used for selected rows, hover emphasis, and primary actions.");
                ui.add(
                    egui::Slider::new(&mut self.theme.panel_rounding, 0..=16)
                        .text("Panel rounding"),
                );
                ui.checkbox(
                    &mut self.theme.list_row_shading,
                    "Use shaded backgrounds for guild/channel rows",
                );
                ui.separator();
                ui.label("Readability");
                ui.add(
                    egui::Slider::new(&mut self.readability.text_scale, 0.8..=1.4)
                        .text("Text scale")
                        .step_by(0.05),
                );
                ui.checkbox(&mut self.readability.compact_density, "Compact UI density");
                ui.checkbox(
                    &mut self.readability.show_timestamps,
                    "Show message timestamps",
                );
                ui.checkbox(
                    &mut self.readability.message_bubble_backgrounds,
                    "Show chat message bubble backgrounds",
                );

                if ui.button("Reset all settings to defaults").clicked() {
                    self.theme = ThemeSettings::discord_default();
                    self.readability = UiReadabilitySettings::defaults();
                }
            });
    }

    fn show_status_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(banner) = self.status_banner.clone() {
            let (fill, stroke) = match banner.severity {
                StatusBannerSeverity::Error => (
                    egui::Color32::from_rgb(111, 53, 53),
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(175, 96, 96)),
                ),
            };

            egui::Frame::none()
                .fill(fill)
                .stroke(stroke)
                .rounding(8.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(&banner.message).color(egui::Color32::WHITE));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Dismiss").clicked() {
                                self.status_banner = None;
                            }
                        });
                    });
                });
        }
    }

    // ---------- Improved login UI helpers (stable IDs + stacked layout) ----------

    fn login_text_field(
        &mut self,
        ui: &mut egui::Ui,
        id: &'static str,
        label: &str,
        hint: &str,
        value: &mut String,
        should_focus: bool,
    ) -> egui::Response {
        ui.label(egui::RichText::new(label).strong());
        let edit = egui::TextEdit::singleline(value)
            .id_source(id)
            .hint_text(
                egui::RichText::new(hint)
                    .color(ui.visuals().weak_text_color().gamma_multiply(0.85)),
            )
            .desired_width(f32::INFINITY);

        // Taller inputs are easier to click and feel “app-like”.
        let response = ui.add_sized([ui.available_width(), 34.0], edit);

        // One-time / directed focus that doesn't flicker.
        if should_focus {
            response.request_focus();
        }

        response
    }
    fn show_login_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Background spacing
            let avail = ui.available_size();
            let card_width = avail.x.clamp(440.0, 560.0);
            let top_space = (avail.y * 0.12).clamp(18.0, 90.0);

            ui.add_space(top_space);

            // Centered card
            ui.vertical_centered(|ui| {
                ui.set_width(card_width);

                let palette = theme_discord_dark_palette(self.theme);
                let card_fill = palette
                    .map(|p| lighten_color(p.app_background, 0.06))
                    .unwrap_or_else(|| lighten_color(ui.visuals().panel_fill, 0.02));

                egui::Frame::none()
                    .fill(card_fill)
                    .rounding(14.0)
                    .stroke(egui::Stroke::new(
                        1.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    ))
                    .inner_margin(egui::Margin::symmetric(20.0, 18.0))
                    .show(ui, |ui| {
                        ui.style_mut().spacing.item_spacing = egui::vec2(10.0, 10.0);

                        // Header
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("💬").size(24.0));
                            ui.vertical(|ui| {
                                ui.heading("Proto RTC");
                                ui.weak("Sign in to your workspace.");
                            });
                        });

                        ui.add_space(8.0);
                        self.show_status_banner(ui);

                        // Determine focus request (once, or after clicking label buttons)
                        let mut focus_to_set = None;
                        if !self.login_ui.attempted_auto_focus {
                            self.login_ui.attempted_auto_focus = true;
                            focus_to_set = self.login_ui.focus;
                        } else if self.login_ui.focus.is_some() {
                            focus_to_set = self.login_ui.focus;
                            self.login_ui.focus = None;
                        }

                        // Fields (stacked)
                        egui::Frame::none()
                            .fill(ui.visuals().faint_bg_color.gamma_multiply(0.55))
                            .rounding(12.0)
                            .inner_margin(egui::Margin::symmetric(14.0, 12.0))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("Account")
                                        .strong()
                                        .size(20.0 * self.readability.text_scale),
                                );
                                ui.add_space(6.0);

                                let mut server_url_buf = self.server_url.clone();
                                let mut username_buf = self.username.clone();

                                // Account fields
                                let server_resp = self.login_text_field(
                                    ui,
                                    "login_server_url",
                                    "Server URL",
                                    "http://127.0.0.1:8443",
                                    &mut server_url_buf,
                                    focus_to_set == Some(LoginFocusField::Server),
                                );

                                ui.add_space(6.0);

                                let user_resp = self.login_text_field(
                                    ui,
                                    "login_username",
                                    "Username",
                                    "alice",
                                    &mut username_buf,
                                    focus_to_set == Some(LoginFocusField::Username),
                                );
                                self.server_url = server_url_buf;
                                self.username = username_buf;
                                if !self.auth_session_established {
                                    self.display_name_draft = self.username.clone();
                                }

                                // Enter submits if username/server has focus
                                let enter_pressed = ctx.input(|i| i.key_pressed(egui::Key::Enter));
                                let can_submit = user_resp.has_focus() || server_resp.has_focus();
                                if can_submit && enter_pressed {
                                    self.try_login();
                                }
                            });

                        ui.add_space(10.0);

                        // Sign in button row
                        ui.horizontal(|ui| {
                            let is_busy = !self.auth_session_established
                                && self.status.to_ascii_lowercase().contains("starting");
                            let mut btn = egui::Button::new(
                                egui::RichText::new("Sign in").strong().size(16.0),
                            )
                            .min_size(egui::vec2(ui.available_width(), 40.0));
                            if let Some(p) = theme_discord_dark_palette(self.theme) {
                                btn = btn
                                    .fill(self.theme.accent_color)
                                    .stroke(egui::Stroke::new(1.0, p.guild_entry_stroke_active));
                            }

                            if ui.add_enabled(!is_busy, btn).clicked() {
                                self.try_login();
                                self.login_ui.last_login_click_tick = self.tick;
                            }
                        });

                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(6.0);

                        ui.horizontal_wrapped(|ui| {
                            ui.small("Status:");
                            ui.small(egui::RichText::new(&self.status).weak());
                        });
                    });
            });

            ui.add_space((avail.y * 0.08).clamp(12.0, 60.0));
        });
    }

    fn sign_out(&mut self) {
        self.auth_session_established = false;
        self.view_state = AppViewState::Login;
        self.status = "Signed out".to_string();
        self.status_banner = None;
    }

    fn try_login(&mut self) {
        let username = self.username.trim().to_string();
        if username.is_empty() {
            self.status = "Username is required for username-only sign in".to_string();
            self.status_banner = Some(StatusBanner {
                severity: StatusBannerSeverity::Error,
                message: "Please enter a username.".to_string(),
            });
            self.login_ui.focus = Some(LoginFocusField::Username);
            return;
        }

        let server = self.server_url.trim().to_string();
        if server.is_empty() {
            self.status = "Server URL is required".to_string();
            self.status_banner = Some(StatusBanner {
                severity: StatusBannerSeverity::Error,
                message: "Please enter a server URL.".to_string(),
            });
            self.login_ui.focus = Some(LoginFocusField::Server);
            return;
        }

        self.auth_session_established = false;
        self.status_banner = None;
        self.display_name_draft = username.clone();
        queue_command(
            &self.cmd_tx,
            BackendCommand::Login {
                server_url: server,
                username,
            },
            &mut self.status,
        );
    }

    fn attachment_preview_cache_key(path: &std::path::Path) -> AttachmentPreviewCacheKey {
        let modified = fs::metadata(path).and_then(|m| m.modified()).ok();
        AttachmentPreviewCacheKey {
            path: path.to_path_buf(),
            modified,
        }
    }

    fn is_previewable_image(path: &std::path::Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                )
            })
            .unwrap_or(false)
    }

    fn load_attachment_preview(
        &mut self,
        ctx: &egui::Context,
        path: &std::path::Path,
    ) -> Option<AttachmentPreview> {
        if !Self::is_previewable_image(path) {
            return None;
        }

        let cache_key = Self::attachment_preview_cache_key(path);
        if let Some(cached) = self.attachment_preview_cache.get(&cache_key).cloned() {
            return Some(cached);
        }

        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.attachment_preview_cache
                    .insert(cache_key, AttachmentPreview::DecodeFailed);
                return Some(AttachmentPreview::DecodeFailed);
            }
        };

        let decoded = match image::load_from_memory(&bytes) {
            Ok(image) => image,
            Err(_) => {
                self.attachment_preview_cache
                    .insert(cache_key, AttachmentPreview::DecodeFailed);
                return Some(AttachmentPreview::DecodeFailed);
            }
        };

        let (orig_w, orig_h) = decoded.dimensions();
        let max_dimension = 240.0_f32;
        let scale = (max_dimension / (orig_w.max(orig_h) as f32)).min(1.0);
        let resized = if scale < 1.0 {
            decoded.resize(
                (orig_w as f32 * scale).max(1.0) as u32,
                (orig_h as f32 * scale).max(1.0) as u32,
                image::imageops::FilterType::Triangle,
            )
        } else {
            decoded
        };
        let rgba = resized.to_rgba8();
        let [w, h] = [rgba.width() as usize, rgba.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
        let texture = ctx.load_texture(
            format!("attachment-preview:{}", path.display()),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        let preview_png = encode_rgba_png(rgba.as_raw(), w, h).unwrap_or_default();
        let preview = AttachmentPreview::Image {
            texture,
            size: egui::vec2(w as f32, h as f32),
            preview_png,
        };
        self.attachment_preview_cache
            .insert(cache_key, preview.clone());
        Some(preview)
    }

    fn attachment_size_text(path: &std::path::Path) -> String {
        let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        human_readable_bytes(bytes)
    }

    fn try_send_current_composer(&mut self, response: &egui::Response) {
        let has_text = !self.composer.trim().is_empty();
        let has_attachment = self.pending_attachment.is_some();
        if has_text || has_attachment {
            if self.selected_text_channel_id().is_none() {
                self.status = "Select a text channel before sending messages".to_string();
                return;
            }

            let text = self.composer.trim_end_matches('\n').to_string();
            self.composer.clear();
            let attachment_path = self.pending_attachment.take();
            queue_command(
                &self.cmd_tx,
                BackendCommand::SendMessage {
                    text,
                    attachment_path,
                },
                &mut self.status,
            );
            response.request_focus();
        }
    }

    fn copy_image_to_clipboard(&mut self, bytes: &[u8], label: &str) {
        match decode_image_for_clipboard(bytes)
            .and_then(|(rgba, width, height)| write_clipboard_image(&rgba, width, height))
        {
            Ok(()) => self.status = format!("Copied {label} to clipboard"),
            Err(err) => self.status = format!("Failed to copy {label}: {err}"),
        }
    }

    fn save_image_bytes_as(&mut self, bytes: &[u8], suggested_name: &str) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(suggested_name)
            .save_file()
        {
            match fs::write(&path, bytes) {
                Ok(()) => {
                    self.status = format!("Saved image to {}", path.display());
                }
                Err(err) => {
                    self.status = format!("Failed to save image: {err}");
                }
            }
        }
    }

    fn open_file_in_external_viewer(&mut self, path: &std::path::Path) {
        #[cfg(target_os = "windows")]
        let result = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn();

        #[cfg(target_os = "macos")]
        let result = std::process::Command::new("open").arg(path).spawn();

        #[cfg(all(unix, not(target_os = "macos")))]
        let result = std::process::Command::new("xdg-open").arg(path).spawn();

        if let Err(err) = result {
            self.status = format!("Failed to open external viewer: {err}");
        }
    }

    fn render_image_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        file_name: &str,
        full_bytes: Option<&[u8]>,
        preview_bytes: Option<&[u8]>,
        external_path: Option<&std::path::Path>,
        metadata: Option<&str>,
    ) {
        ui.set_min_width(260.0);
        ui.label(egui::RichText::new("Image actions").strong());
        ui.small("Full quality uses original bytes. Preview quality uses the rendered thumbnail.");
        ui.separator();

        let copy_full = ui
            .add_enabled(
                full_bytes.is_some(),
                egui::Button::new("Copy image (full quality)"),
            )
            .on_hover_text("Best quality; may not be available yet if bytes are still loading.")
            .clicked();
        if copy_full {
            if let Some(bytes) = full_bytes {
                self.copy_image_to_clipboard(bytes, "full-quality image");
            }
            ui.close_menu();
        }

        let copy_preview = ui
            .add_enabled(
                preview_bytes.is_some(),
                egui::Button::new("Copy image (preview quality)"),
            )
            .on_hover_text("Copies the downscaled preview exactly as shown in chat.")
            .clicked();
        if copy_preview {
            if let Some(bytes) = preview_bytes {
                self.copy_image_to_clipboard(bytes, "preview image");
            }
            ui.close_menu();
        }

        let save_full = ui
            .add_enabled(full_bytes.is_some(), egui::Button::new("Save image as…"))
            .on_hover_text("Saves original bytes when available.")
            .clicked();
        if save_full {
            if let Some(bytes) = full_bytes {
                self.save_image_bytes_as(bytes, file_name);
            }
            ui.close_menu();
        }

        let open_external = ui
            .add_enabled(
                external_path.is_some(),
                egui::Button::new("Open image in external viewer"),
            )
            .on_hover_text("Uses your platform default image viewer.")
            .clicked();
        if open_external {
            if let Some(path) = external_path {
                self.open_file_in_external_viewer(path);
            }
            ui.close_menu();
        }

        ui.separator();
        if ui.button("Copy file name").clicked() {
            ui.ctx().copy_text(file_name.to_string());
            self.status = "Copied file name to clipboard".to_string();
            ui.close_menu();
        }

        if ui
            .add_enabled(metadata.is_some(), egui::Button::new("Copy metadata"))
            .clicked()
        {
            if let Some(text) = metadata {
                ui.ctx().copy_text(text.to_string());
                self.status = "Copied image metadata to clipboard".to_string();
            }
            ui.close_menu();
        }
    }

    #[allow(dead_code)]
    fn render_left_user_panel(&mut self, ui: &mut egui::Ui) {
        let discord_dark = theme_discord_dark_palette(self.theme);
        egui::Frame::group(ui.style())
            .fill(
                discord_dark
                    .map(|p| p.message_background)
                    .unwrap_or(ui.visuals().panel_fill),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("●").color(egui::Color32::from_rgb(67, 181, 129)));
                    ui.strong(&self.username);
                    let status_label = if self.voice_ui.active_session.is_some() {
                        "In voice"
                    } else {
                        "Online"
                    };
                    ui.small(format!("· {status_label}"));
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.toggle_value(&mut self.voice_ui.muted, "Mute");
                    ui.toggle_value(&mut self.voice_ui.deafened, "Deafen");
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.settings_open = true;
                    }
                    if ui.button("⎋ Sign out").clicked() {
                        self.auth_session_established = false;
                        self.view_state = AppViewState::Login;
                        self.status = "Signed out".to_string();
                        self.status_banner = None;
                    }
                });
            });
    }

    // ------------------- Main workspace (mostly unchanged) -------------------

    fn show_account_menu_contents(&mut self, ui: &mut egui::Ui) {
        let auth_ready = self.auth_session_established;
        let backend_supports_profile_edit = false;

        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 1.0);
        ui.style_mut().visuals.widgets.inactive.rounding = egui::Rounding::same(0.0);
        ui.style_mut().visuals.widgets.hovered.rounding = egui::Rounding::same(0.0);
        ui.style_mut().visuals.widgets.active.rounding = egui::Rounding::same(0.0);
        ui.style_mut().visuals.widgets.open.rounding = egui::Rounding::same(0.0);

        ui.set_min_width(260.0);
        ui.label(egui::RichText::new(&self.username).strong());
        ui.small(format!(
            "{} ({})",
            self.server_url,
            server_environment_label(&self.server_url)
        ));
        ui.small(format!(
            "Session: {}",
            if auth_ready {
                "Authenticated"
            } else {
                "Signed out"
            }
        ));

        ui.separator();
        ui.small("Display name");
        ui.add(
            egui::TextEdit::singleline(&mut self.display_name_draft)
                .hint_text("Display name")
                .desired_width(f32::INFINITY),
        );
        let save_display_name = ui
            .add_enabled(
                auth_ready && backend_supports_profile_edit,
                egui::Button::new("Save display name"),
            )
            .on_hover_text("Backend command support is required for profile updates.");
        if save_display_name.clicked() {
            self.status = "Display name updated".to_string();
            ui.close_menu();
        }

        ui.separator();
        ui.small("Presence");
        egui::ComboBox::from_id_source("account_presence_combo")
            .selected_text(self.presence_preference.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.presence_preference,
                    AccountPresence::Online,
                    AccountPresence::Online.label(),
                );
                ui.selectable_value(
                    &mut self.presence_preference,
                    AccountPresence::Away,
                    AccountPresence::Away.label(),
                );
                ui.selectable_value(
                    &mut self.presence_preference,
                    AccountPresence::DoNotDisturb,
                    AccountPresence::DoNotDisturb.label(),
                );
            });

        ui.separator();
        ui.small("Notifications");
        ui.checkbox(&mut self.notifications_enabled, "Enable notifications");
        ui.add_enabled_ui(self.notifications_enabled, |ui| {
            ui.checkbox(&mut self.desktop_notifications_enabled, "Desktop alerts");
            ui.checkbox(&mut self.mention_notifications_enabled, "Mentions only");
        });

        ui.separator();
        let sign_out = ui
            .add_enabled(auth_ready, egui::Button::new("Sign out"))
            .on_disabled_hover_text("No active session to sign out from.");
        if sign_out.clicked() {
            self.sign_out();
            ui.close_menu();
        }
    }

    fn refresh_selected_guild_navigation(&mut self) {
        if let Some(guild_id) = self.selected_guild {
            queue_command(
                &self.cmd_tx,
                BackendCommand::ListChannels { guild_id },
                &mut self.status,
            );
            queue_command(
                &self.cmd_tx,
                BackendCommand::ListMembers { guild_id },
                &mut self.status,
            );
        } else {
            self.status = "Select a guild first".to_string();
        }
    }

    fn show_channels_panel_header(
        &mut self,
        ui: &mut egui::Ui,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        ui.horizontal(|ui| {
            ui.heading("Channels");
            let refresh_label = if let Some(palette) = discord_dark {
                egui::RichText::new("Refresh").color(palette.side_panel_button_text)
            } else {
                egui::RichText::new("Refresh")
            };
            let refresh_button = self.sidebar_button(refresh_label, discord_dark);
            if ui.add(refresh_button).clicked() {
                self.refresh_selected_guild_navigation();
            }
        });
    }

    fn show_voice_channel_action(
        &mut self,
        ui: &mut egui::Ui,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        if let Some(channel) = self.selected_voice_channel().cloned() {
            ui.separator();
            let connected_here = self.voice_ui.is_channel_connected(channel.channel_id);
            let cta_label = if connected_here {
                "Leave voice"
            } else {
                "Join voice"
            };
            let cta_text = if let Some(palette) = discord_dark {
                egui::RichText::new(cta_label).color(palette.side_panel_button_text)
            } else {
                egui::RichText::new(cta_label)
            };
            let cta_button = self
                .sidebar_button(cta_text, discord_dark)
                .min_size(egui::vec2(ui.available_width(), 30.0));
            if ui.add(cta_button).clicked() {
                if connected_here {
                    self.voice_ui.connection_status = VoiceSessionConnectionStatus::Disconnecting;
                    queue_command(
                        &self.cmd_tx,
                        BackendCommand::DisconnectVoice,
                        &mut self.status,
                    );
                } else {
                    self.voice_ui.connection_status = VoiceSessionConnectionStatus::Connecting;
                    self.voice_ui.last_error = None;
                    queue_command(
                        &self.cmd_tx,
                        BackendCommand::ConnectVoice {
                            guild_id: channel.guild_id,
                            channel_id: channel.channel_id,
                        },
                        &mut self.status,
                    );
                }
            }
        }
    }

    fn main_workspace_style(&self, ctx: &egui::Context) -> MainWorkspaceStyle {
        let mut layout = MainWorkspaceLayout::default();
        if self.readability.compact_density {
            layout.toolbar_h_padding = 8.0;
            layout.toolbar_v_padding = 6.0;
            layout.section_vertical_gap = 6.0;
            layout.channel_row_height = 30.0;
        }
        let discord_dark = theme_discord_dark_palette(self.theme);
        let colors = MainWorkspaceColors {
            top_bar_bg: discord_dark
                .map(|p| p.navigation_background)
                .unwrap_or(ctx.style().visuals.panel_fill),
            nav_bg: discord_dark
                .map(|p| p.navigation_background)
                .unwrap_or(ctx.style().visuals.panel_fill),
            members_bg: discord_dark
                .map(|p| p.members_background)
                .unwrap_or(ctx.style().visuals.panel_fill),
            message_bg: discord_dark
                .map(|p| p.message_background)
                .unwrap_or(ctx.style().visuals.panel_fill),
        };

        MainWorkspaceStyle {
            layout,
            colors,
            discord_dark,
        }
    }

    fn sidebar_button(
        &self,
        label: impl Into<egui::WidgetText>,
        discord_dark: Option<DiscordDarkPalette>,
    ) -> egui::Button<'static> {
        let mut button = egui::Button::new(label);
        if let Some(palette) = discord_dark {
            button = button
                .fill(palette.side_panel_button_fill)
                .stroke(egui::Stroke::new(1.0, palette.guild_entry_stroke));
        }
        button
    }

    fn render_nav_row(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        row_height: f32,
        selected: bool,
        discord_dark: Option<DiscordDarkPalette>,
    ) -> egui::Response {
        let base_bg = discord_dark
            .map(|p| p.navigation_background)
            .unwrap_or_else(|| {
                if self.theme.list_row_shading {
                    ui.visuals().faint_bg_color
                } else {
                    egui::Color32::TRANSPARENT
                }
            });
        let selected_bg = discord_dark
            .map(|p| p.guild_entry_active)
            .unwrap_or_else(|| {
                ui.visuals()
                    .selection
                    .bg_fill
                    .gamma_multiply(if self.theme.list_row_shading {
                        0.35
                    } else {
                        0.22
                    })
            });
        let row_stroke = discord_dark
            .map(|palette| {
                egui::Stroke::new(
                    1.0,
                    if selected {
                        palette.guild_entry_stroke_active
                    } else {
                        palette.guild_entry_stroke
                    },
                )
            })
            .unwrap_or_else(|| {
                if selected {
                    egui::Stroke::new(1.0, ui.visuals().selection.bg_fill.gamma_multiply(0.9))
                } else if self.theme.list_row_shading {
                    egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
                } else {
                    egui::Stroke::NONE
                }
            });

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click(),
        );
        let row_fill = if selected {
            selected_bg
        } else if response.hovered() {
            discord_dark
                .map(|p| p.guild_entry_hover)
                .unwrap_or_else(|| ui.visuals().widgets.hovered.bg_fill)
        } else {
            base_bg
        };

        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(f32::from(self.theme.panel_rounding)),
            row_fill,
        );
        if row_stroke != egui::Stroke::NONE {
            ui.painter().rect_stroke(
                rect,
                egui::Rounding::same(f32::from(self.theme.panel_rounding)),
                row_stroke,
            );
        }

        let text_color = if selected {
            discord_dark
                .map(|p| p.guild_text_highlighted)
                .unwrap_or(ui.visuals().strong_text_color())
        } else if response.hovered() {
            discord_dark
                .map(|p| p.guild_text_hovered)
                .unwrap_or(ui.visuals().strong_text_color())
        } else {
            discord_dark
                .map(|p| p.guild_text_unhighlighted)
                .unwrap_or(ui.visuals().text_color())
        };

        ui.painter().text(
            rect.left_center() + egui::vec2(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::TextStyle::Button.resolve(ui.style()),
            text_color,
        );

        response
    }

    fn show_left_navigation_panel(&mut self, ctx: &egui::Context, style: MainWorkspaceStyle) {
        let nav_width = style.layout.guilds_panel_width + style.layout.channels_panel_width + 24.0;
        egui::SidePanel::left("left_navigation_panel")
            .default_width(nav_width)
            .frame(egui::Frame::none().fill(style.colors.nav_bg).inner_margin(
                egui::Margin::symmetric(
                    style.layout.toolbar_h_padding,
                    style.layout.toolbar_v_padding,
                ),
            ))
            .show(ctx, |ui| {
                let discord_dark = style.discord_dark;

                // Layout contract:
                // 1) Top region holds guild + channel columns.
                // 2) Bottom region holds user info and is never allowed to overlap top content.
                egui::TopBottomPanel::bottom("left_nav_user_strip")
                    .resizable(self.auth_session_established)
                    .default_height(
                        self.left_user_panel_height
                            .clamp(MIN_LEFT_USER_PANEL_HEIGHT, MAX_LEFT_USER_PANEL_HEIGHT),
                    )
                    .min_height(MIN_LEFT_USER_PANEL_HEIGHT)
                    .max_height(MAX_LEFT_USER_PANEL_HEIGHT)
                    .show_inside(ui, |ui| {
                        if self.auth_session_established {
                            self.left_user_panel_height = ui
                                .available_height()
                                .clamp(MIN_LEFT_USER_PANEL_HEIGHT, MAX_LEFT_USER_PANEL_HEIGHT);
                            self.render_left_user_panel(ui);
                        }
                    });

                ui.add_space(style.layout.section_vertical_gap);

                // Top navigation area: keep columns independent and scrollable.
                // Fill this entire remaining region first, then split horizontally.
                ui.allocate_ui_with_layout(
                    ui.available_size(),
                    egui::Layout::left_to_right(egui::Align::Min),
                    |ui| {
                        let guilds_width = style.layout.guilds_panel_width;

                        ui.allocate_ui_with_layout(
                            egui::vec2(guilds_width, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.heading("Guilds");
                                ui.add_space(style.layout.section_vertical_gap);

                                egui::ScrollArea::vertical()
                                    .id_source("left_nav_guilds_scroll")
                                    .auto_shrink([false, false])
                                    .max_height(ui.available_height())
                                    .show(ui, |ui| {
                                        ui.add_enabled_ui(self.auth_session_established, |ui| {
                                            if let Some(guild_id) = self.selected_guild {
                                                let label = if let Some(palette) = discord_dark {
                                                    egui::RichText::new("Create Invite")
                                                        .color(palette.side_panel_button_text)
                                                } else {
                                                    egui::RichText::new("Create Invite")
                                                };
                                                let invite_button = self
                                                    .sidebar_button(label, discord_dark)
                                                    .min_size(egui::vec2(
                                                        ui.available_width(),
                                                        30.0,
                                                    ));
                                                if ui.add(invite_button).clicked() {
                                                    queue_command(
                                                        &self.cmd_tx,
                                                        BackendCommand::CreateInvite { guild_id },
                                                        &mut self.status,
                                                    );
                                                }
                                                ui.add_space(8.0);
                                            }

                                            for guild in &self.guilds {
                                                let selected =
                                                    self.selected_guild == Some(guild.guild_id);
                                                let response = self.render_nav_row(
                                                    ui,
                                                    guild.name.as_str(),
                                                    style.layout.channel_row_height,
                                                    selected,
                                                    discord_dark,
                                                );

                                                if response.clicked() {
                                                    self.selected_guild = Some(guild.guild_id);
                                                    self.selected_channel = None;
                                                    self.channels.clear();
                                                    self.members.remove(&guild.guild_id);
                                                    queue_command(
                                                        &self.cmd_tx,
                                                        BackendCommand::ListChannels {
                                                            guild_id: guild.guild_id,
                                                        },
                                                        &mut self.status,
                                                    );
                                                    queue_command(
                                                        &self.cmd_tx,
                                                        BackendCommand::ListMembers {
                                                            guild_id: guild.guild_id,
                                                        },
                                                        &mut self.status,
                                                    );
                                                }

                                                ui.add_space(6.0);
                                            }
                                        });
                                    });

                                if !self.auth_session_established {
                                    ui.separator();
                                    ui.add_space(4.0);
                                    ui.label("Sign in to access guilds.");
                                }
                            },
                        );

                        ui.separator();

                        ui.allocate_ui_with_layout(
                            ui.available_size(),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                self.show_channels_panel_header(ui, discord_dark);
                                ui.add_space(style.layout.section_vertical_gap);

                                egui::ScrollArea::vertical()
                                    .id_source("left_nav_channels_scroll")
                                    .auto_shrink([false, false])
                                    .max_height(ui.available_height())
                                    .show(ui, |ui| {
                                        ui.add_enabled_ui(self.auth_session_established, |ui| {
                                            for index in 0..self.channels.len() {
                                                let channel = &self.channels[index];
                                                let channel_id = channel.channel_id;
                                                let channel_kind = channel.kind;
                                                let channel_name = channel.name.clone();
                                                self.render_channel_row(
                                                    ui,
                                                    channel_id,
                                                    channel_kind,
                                                    &channel_name,
                                                    style.layout.channel_row_height,
                                                    discord_dark,
                                                );
                                            }
                                            self.show_voice_channel_action(ui, discord_dark);
                                        });

                                        if !self.auth_session_established {
                                            ui.separator();
                                            ui.add_space(4.0);
                                            ui.label("Sign in to browse channels.");
                                        }
                                    });
                            },
                        );
                    },
                );
            });
    }

    fn render_channel_row(
        &mut self,
        ui: &mut egui::Ui,
        channel_id: ChannelId,
        kind: ChannelKind,
        name: &str,
        row_height: f32,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        let icon = match kind {
            ChannelKind::Text => "#",
            ChannelKind::Voice => "🔊",
        };
        let selected = self.selected_channel == Some(channel_id);

        let occupancy =
            (kind == ChannelKind::Voice).then(|| self.voice_ui.occupancy_for_channel(channel_id));
        let connection_hint = self
            .voice_ui
            .is_channel_connected(channel_id)
            .then_some("connected");

        let channel_label = match (occupancy, connection_hint) {
            (Some(count), Some(hint)) => format!("{icon}  {name} ({count}) {hint}"),
            (Some(count), None) => format!("{icon}  {name} ({count})"),
            (None, _) => format!("{icon}  {name}"),
        };
        let response = self.render_nav_row(ui, &channel_label, row_height, selected, discord_dark);

        if response.clicked() {
            self.selected_channel = Some(channel_id);
            if kind == ChannelKind::Voice {
                self.voice_ui.selected_voice_channel = Some(channel_id);
            }
            queue_command(
                &self.cmd_tx,
                BackendCommand::SelectChannel { channel_id },
                &mut self.status,
            );
        }

        ui.add_space(6.0);
    }

    fn show_members_side_panel(&mut self, ctx: &egui::Context, style: MainWorkspaceStyle) {
        egui::SidePanel::right("members_panel")
            .default_width(style.layout.members_panel_width)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(style.colors.members_bg)
                    .inner_margin(egui::Margin::symmetric(
                        style.layout.toolbar_h_padding,
                        style.layout.toolbar_v_padding,
                    ))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.heading("Members");
                            if ui.button("Refresh").clicked() {
                                if let Some(guild_id) = self.selected_guild {
                                    queue_command(
                                        &self.cmd_tx,
                                        BackendCommand::ListMembers { guild_id },
                                        &mut self.status,
                                    );
                                }
                            }
                        });
                        ui.add_space(style.layout.section_vertical_gap);

                        if let Some(guild_id) = self.selected_guild {
                            if let Some(members) = self.members.get(&guild_id) {
                                if members.is_empty() {
                                    ui.label("No visible members in this guild yet.");
                                } else {
                                    for member in members {
                                        let role_label = match member.role {
                                            Role::Owner => "owner",
                                            Role::Mod => "mod",
                                            Role::Member => "member",
                                        };
                                        let mute_label =
                                            if member.muted { " · 🔇 muted" } else { "" };
                                        ui.label(format!(
                                            "{} ({}){}",
                                            member.username, role_label, mute_label
                                        ));
                                    }
                                }
                            } else {
                                ui.label("Loading members for selected guild…");
                            }
                        } else {
                            ui.label("Select a guild to view members.");
                        }

                        ui.separator();
                        ui.add_space(style.layout.section_vertical_gap);
                        ui.heading("Voice");
                        ui.add_space(4.0);
                        let (badge_text, badge_color) = self.voice_status_badge();
                        ui.colored_label(badge_color, format!("Status: {badge_text}"));

                        if let Some(channel) = self.selected_voice_channel() {
                            ui.label(format!("Selected: {}", channel.name));
                            ui.small(format!(
                                "Occupancy: {} participant(s)",
                                self.voice_ui.occupancy_for_channel(channel.channel_id)
                            ));
                        }

                        if let Some(active) = &self.voice_ui.active_session {
                            let active_name = self
                                .channels
                                .iter()
                                .find(|channel| channel.channel_id == active.channel_id)
                                .map(|channel| channel.name.as_str())
                                .unwrap_or("Unknown voice channel");
                            ui.label(format!("Connected channel: {active_name}"));
                            ui.label(format!(
                                "Participants: {}",
                                self.voice_ui.active_participant_count()
                            ));
                            ui.horizontal_wrapped(|ui| {
                                ui.toggle_value(&mut self.voice_ui.muted, "Mute");
                                ui.toggle_value(&mut self.voice_ui.deafened, "Deafen");
                            });
                        } else {
                            ui.label("Not connected to voice.");
                        }

                        if let Some(err) = &self.voice_ui.last_error {
                            ui.add_space(4.0);
                            ui.colored_label(
                                egui::Color32::RED,
                                format!("Last voice error: {err}"),
                            );
                        }
                    });
            });
    }

    fn show_top_bar(&mut self, ctx: &egui::Context, style: MainWorkspaceStyle) {
        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::none()
                    .fill(style.colors.top_bar_bg)
                    .inner_margin(egui::Margin::symmetric(
                        style.layout.top_bar_h_padding,
                        style.layout.top_bar_v_padding,
                    )),
            )
            .show(ctx, |ui| {
                ui.scope(|ui| {
                    let style = ui.style_mut();
                    style.spacing.button_padding = egui::vec2(4.0, 0.0);
                    style.spacing.item_spacing.x = 0.0;
                    style.spacing.interact_size.y = 18.0;
                    style.visuals.widgets.inactive.rounding = egui::Rounding::same(0.0);
                    style.visuals.widgets.hovered.rounding = egui::Rounding::same(0.0);
                    style.visuals.widgets.active.rounding = egui::Rounding::same(0.0);
                    style.visuals.widgets.open.rounding = egui::Rounding::same(0.0);

                    egui::menu::bar(ui, |ui| {
                        ui.menu_button("⚙ Settings", |ui| {
                            ui.style_mut().spacing.button_padding = egui::vec2(4.0, 0.0);
                            ui.style_mut().visuals.widgets.inactive.rounding =
                                egui::Rounding::same(0.0);
                            ui.style_mut().visuals.widgets.hovered.rounding =
                                egui::Rounding::same(0.0);
                            ui.style_mut().visuals.widgets.active.rounding =
                                egui::Rounding::same(0.0);
                            ui.style_mut().visuals.widgets.open.rounding =
                                egui::Rounding::same(0.0);

                            ui.label(egui::RichText::new("Quick settings").weak());
                            ui.checkbox(&mut self.readability.compact_density, "Compact density");
                            ui.checkbox(
                                &mut self.readability.show_timestamps,
                                "Message timestamps",
                            );
                            ui.checkbox(&mut self.notifications_enabled, "Enable notifications");
                            ui.separator();

                            if ui.button("Advanced settings…").clicked() {
                                self.settings_open = true;
                                ui.close_menu();
                            }
                        });

                        ui.menu_button("Account", |ui| self.show_account_menu_contents(ui));

                        if ui.button("Refresh Guilds").clicked() {
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::ListGuilds,
                                &mut self.status,
                            );
                        }

                        ui.separator();
                        ui.label("Invite:");

                        let invite_width = ui.available_width().clamp(120.0, 220.0);
                        let invite_input = ui
                            .scope(|ui| {
                                if theme_discord_dark_palette(self.theme).is_some() {
                                    ui.visuals_mut().override_text_color = None;
                                }
                                ui.add_sized(
                                    [invite_width, ui.spacing().interact_size.y],
                                    egui::TextEdit::singleline(&mut self.invite_code_input)
                                        .id_source("main_invite_input")
                                        .hint_text("Enter invite code"),
                                )
                            })
                            .inner;

                        let join_enabled = self.auth_session_established
                            && !self.invite_code_input.trim().is_empty();
                        let join_clicked = ui
                            .add_enabled(join_enabled, egui::Button::new("Join Invite"))
                            .clicked();
                        let join_with_enter = invite_input.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && join_enabled;
                        if join_clicked || join_with_enter {
                            let invite_code = self.invite_code_input.trim().to_string();
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::JoinWithInvite { invite_code },
                                &mut self.status,
                            );
                        }

                        if ui.button("Create Invite").clicked() {
                            if let Some(guild_id) = self.selected_guild {
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::CreateInvite { guild_id },
                                    &mut self.status,
                                );
                            } else {
                                self.status =
                                    "Select a workspace before creating an invite".to_string();
                            }
                        }
                    });
                });

                ui.label(&self.status);
                self.show_status_banner(ui);
            });
    }

    fn show_main_workspace(&mut self, ctx: &egui::Context) {
        let style = self.main_workspace_style(ctx);

        self.show_top_bar(ctx, style);

        self.show_settings_window(ctx);

        self.show_left_navigation_panel(ctx, style);

        self.show_members_side_panel(ctx, style);

        egui::TopBottomPanel::bottom("composer_panel")
            .resizable(true)
            .default_height(self.composer_panel_height)
            .min_height(MIN_COMPOSER_PANEL_HEIGHT)
            .show(ctx, |ui| {
                self.composer_panel_height = ui
                    .available_height()
                    .clamp(MIN_COMPOSER_PANEL_HEIGHT, MAX_COMPOSER_PANEL_HEIGHT);

                ui.add_space(6.0);
                let can_send = self.selected_channel.is_some();

                ui.vertical(|ui| {
                    ui.add_enabled_ui(can_send && self.auth_session_established, |ui| {
                            let row_height = (self.composer_panel_height - 18.0)
                                .clamp(36.0, MAX_COMPOSER_PANEL_HEIGHT - 12.0);
                            let send_width = (88.0 + (row_height - 36.0) * 0.28).clamp(88.0, 124.0);
                            let attachment_width =
                                (38.0 + (row_height - 36.0) * 0.12).clamp(38.0, 48.0);

                            ui.horizontal(|ui| {
                                if ui.button("📎").on_hover_text("Attach file").clicked() {
                                    self.pending_attachment = rfd::FileDialog::new().pick_file();
                                }
                                let response = ui
                                    .scope(|ui| {
                                        if let Some(palette) = theme_discord_dark_palette(self.theme)
                                        {
                                            ui.visuals_mut().extreme_bg_color =
                                                palette.message_background;
                                        }
                                        ui.add_sized(
                                            [
                                                ui.available_width() - send_width - attachment_width,
                                                row_height,
                                            ],
                                            egui::TextEdit::multiline(&mut self.composer)
                                                .id_source("composer_text")
                                                .hint_text(
                                                    "Message #channel (Enter to send, Shift+Enter for newline)",
                                                ),
                                        )
                                    })
                                    .inner;
                                let send_shortcut = response.has_focus()
                                    && ui.input(|i| {
                                        i.key_pressed(egui::Key::Enter) && !i.modifiers.shift
                                    });
                                let clicked_send = ui
                                    .add_sized([send_width, row_height], egui::Button::new("⬆ Send"))
                                    .clicked();
                                if send_shortcut || clicked_send {
                                    self.try_send_current_composer(&response);
                                }
                            });

                            if let Some(path) = self.pending_attachment.clone() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.small(format!("Attached: {}", path.display()));
                                    if ui.button("✕ Remove").clicked() {
                                        self.pending_attachment = None;
                                    }
                                });

                                let file_name = path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("attachment");
                                let size_text = Self::attachment_size_text(&path);
                                match self.load_attachment_preview(ctx, &path) {
                                    Some(AttachmentPreview::Image {
                                        texture,
                                        size,
                                        preview_png,
                                    }) => {
                                        egui::Frame::group(ui.style()).show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.label(format!("🖼 {file_name}"));
                                                ui.small(size_text.clone());
                                            });
                                            let response = ui.add(
                                                egui::Image::new((texture.id(), size))
                                                    .max_size(egui::vec2(240.0, 240.0)),
                                            );
                                            let metadata =
                                                format!("name: {file_name}\nsize: {size_text}");
                                            response.context_menu(|ui| {
                                                self.render_image_context_menu(
                                                    ui,
                                                    file_name,
                                                    fs::read(&path).ok().as_deref(),
                                                    Some(preview_png.as_slice()),
                                                    Some(path.as_path()),
                                                    Some(&metadata),
                                                );
                                            });
                                        });
                                    }
                                    Some(AttachmentPreview::DecodeFailed) => {
                                        egui::Frame::group(ui.style()).show(ui, |ui| {
                                            ui.label(format!("⚠ {file_name}"));
                                            ui.small(size_text.clone());
                                            ui.small("Image preview unavailable (decode failed)");
                                        });
                                    }
                                    None => {
                                        egui::Frame::group(ui.style()).show(ui, |ui| {
                                            ui.label(format!("📎 {file_name}"));
                                            ui.small(size_text);
                                        });
                                    }
                                }
                                ui.add_space(6.0);
                            }
                    });
                });

                if !can_send {
                    ui.centered_and_justified(|ui| {
                        ui.weak("Pick a channel to start chatting.");
                    });
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(style.colors.message_bg).inner_margin(
                egui::Margin::symmetric(0.0, style.layout.toolbar_v_padding),
            ))
            .show(ctx, |ui| {
                ui.add_space(style.layout.toolbar_h_padding);
                ui.horizontal(|ui| {
                    ui.heading("Messages");
                    if let Some(channel_id) = self.selected_channel {
                        if ui.button("Load older").clicked() {
                            if let Some(before) = self.oldest_message_id(channel_id) {
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::LoadMoreMessages { channel_id, before },
                                    &mut self.status,
                                );
                            }
                        }
                    }
                });
                ui.add_space(style.layout.section_vertical_gap);
                ui.separator();
                ui.add_space(2.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(style.layout.toolbar_h_padding);
                    let mut hovered_message = None;
                    if let Some(channel_id) = self.selected_channel {
                        if let Some(messages) = self.messages.get(&channel_id).cloned() {
                            for msg in &messages {
                                let sender_display = msg
                                    .wire
                                    .sender_username
                                    .clone()
                                    .or_else(|| {
                                        self.sender_directory.get(&msg.wire.sender_id.0).cloned()
                                    })
                                    .unwrap_or_else(|| msg.wire.sender_id.0.to_string());
                                let sent_at =
                                    msg.wire.sent_at.format("%Y-%m-%d %H:%M:%S UTC").to_string();

                                let message_margin = if self.readability.compact_density {
                                    egui::Margin::symmetric(8.0, 6.0)
                                } else {
                                    egui::Margin::symmetric(10.0, 8.0)
                                };
                                let discord_dark = theme_discord_dark_palette(self.theme);
                                let base_message_bg =
                                    discord_dark.map(|p| p.message_background).unwrap_or_else(
                                        || ui.visuals().faint_bg_color.gamma_multiply(0.45),
                                    );
                                let hover_message_bg = lighten_color(base_message_bg, 0.12);
                                let row_bg = if self.hovered_message == Some(msg.wire.message_id) {
                                    hover_message_bg
                                } else {
                                    base_message_bg
                                };
                                let frame = if self.readability.message_bubble_backgrounds {
                                    egui::Frame::none().fill(row_bg)
                                } else {
                                    egui::Frame::none()
                                };

                                ui.allocate_ui_with_layout(
                                        egui::vec2(ui.available_width(), 0.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            let message_response = frame
                                                .rounding(egui::Rounding::same(f32::from(
                                                    self.theme.panel_rounding,
                                                )))
                                                .inner_margin(message_margin)
                                                .show(ui, |ui| {
                                                    ui.set_width(ui.available_width());
                                                    ui.horizontal_wrapped(|ui| {
                                                        ui.label(
                                                            egui::RichText::new(sender_display)
                                                                .strong(),
                                                        );
                                                        if self.readability.show_timestamps {
                                                            ui.label(
                                                                egui::RichText::new(sent_at)
                                                                    .small()
                                                                    .weak(),
                                                            );
                                                        }
                                                    });
                                                    ui.label(&msg.plaintext);
                                                    if let Some(attachment) = &msg.wire.attachment {
                                                        if attachment_is_image(attachment) {
                                                            self.render_image_attachment_preview(
                                                                ui, attachment,
                                                            );
                                                        } else {
                                                            ui.horizontal(|ui| {
                                                                ui.label(format!(
                                                                    "📎 {} ({})",
                                                                    attachment.filename,
                                                                    human_readable_bytes(
                                                                        attachment.size_bytes
                                                                    )
                                                                ));
                                                                if ui.button("Download").clicked()
                                                                {
                                                                    queue_command(
                                                                        &self.cmd_tx,
                                                                        BackendCommand::DownloadAttachment {
                                                                            file_id: attachment.file_id,
                                                                            filename: attachment
                                                                                .filename
                                                                                .clone(),
                                                                        },
                                                                        &mut self.status,
                                                                    );
                                                                }
                                                            });
                                                        }
                                                    }
                                                })
                                                .response;
                                            if message_response.hovered() {
                                                hovered_message = Some(msg.wire.message_id);
                                            }
                                        },
                                    );

                                ui.add_space(if self.readability.compact_density {
                                    6.0
                                } else {
                                    8.0
                                });
                            }
                        } else {
                            ui.allocate_ui_with_layout(
                                ui.available_size(),
                                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                                |ui| {
                                    ui.heading("Select a channel");
                                    ui.weak(
                                        "Choose a channel from the left to view and send messages.",
                                    );
                                },
                            );
                        }
                    }
                    self.hovered_message = hovered_message;
                });

                if !self.auth_session_established {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Chat is disabled until an authenticated session is established.",
                    );
                }
            });

        self.render_expanded_preview_window(ctx);
    }

    fn render_image_attachment_preview(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &AttachmentPayload,
    ) {
        let state = self
            .attachment_previews
            .entry(attachment.file_id)
            .or_insert(AttachmentPreviewState::NotRequested);

        if matches!(state, AttachmentPreviewState::NotRequested) {
            *state = AttachmentPreviewState::Loading;
            queue_command(
                &self.cmd_tx,
                BackendCommand::FetchAttachmentPreview {
                    file_id: attachment.file_id,
                },
                &mut self.status,
            );
        }

        match state {
            AttachmentPreviewState::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Loading preview for {}…", attachment.filename));
                });
            }
            AttachmentPreviewState::Error(reason) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Couldn't preview {}: {reason}", attachment.filename),
                );
                self.render_attachment_download_row(ui, attachment);
            }
            AttachmentPreviewState::Ready {
                image,
                original_bytes,
                preview_png,
                texture,
            } => {
                if texture.is_none() {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width, image.height],
                        &image.rgba,
                    );
                    *texture = Some(ui.ctx().load_texture(
                        format!("attachment_preview_{}", attachment.file_id.0),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                }

                let texture_handle = texture.as_ref().cloned();
                let preview_image = image.clone();
                let full_bytes = original_bytes.clone();
                let preview_bytes = preview_png.clone();

                if let Some(texture) = texture_handle {
                    let max_width = (ui.available_width() * 0.75).clamp(120.0, 380.0);
                    let mut preview_size = texture.size_vec2();
                    if preview_size.x > max_width {
                        preview_size *= max_width / preview_size.x;
                    }
                    preview_size.y = preview_size.y.min(260.0);

                    let response = ui
                        .add(
                            egui::ImageButton::new(
                                egui::Image::new(&texture).fit_to_exact_size(preview_size),
                            )
                            .frame(false),
                        )
                        .on_hover_text(
                            "Left click to open large preview · Right click for actions",
                        );

                    if response.clicked() {
                        self.expanded_preview = Some(attachment.file_id);
                    }

                    let metadata = format!(
                        "name: {}\nsize: {} bytes\npreview: {}x{}",
                        attachment.filename,
                        attachment.size_bytes,
                        preview_image.width,
                        preview_image.height
                    );
                    response.context_menu(|ui| {
                        self.render_image_context_menu(
                            ui,
                            &attachment.filename,
                            Some(full_bytes.as_slice()),
                            preview_bytes.as_deref(),
                            None,
                            Some(&metadata),
                        );
                    });

                    ui.small(format!(
                        "🖼 {} ({})",
                        attachment.filename,
                        human_readable_bytes(attachment.size_bytes)
                    ));
                }

                self.render_attachment_download_row(ui, attachment);
            }
            AttachmentPreviewState::NotRequested => {}
        }
    }

    fn render_attachment_download_row(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &AttachmentPayload,
    ) {
        ui.horizontal(|ui| {
            if ui.button("Download original").clicked() {
                queue_command(
                    &self.cmd_tx,
                    BackendCommand::DownloadAttachment {
                        file_id: attachment.file_id,
                        filename: attachment.filename.clone(),
                    },
                    &mut self.status,
                );
            }
        });
    }

    fn render_expanded_preview_window(&mut self, ctx: &egui::Context) {
        let Some(file_id) = self.expanded_preview else {
            return;
        };

        let mut keep_open = true;
        egui::Window::new("Attachment preview")
            .open(&mut keep_open)
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(AttachmentPreviewState::Ready {
                    texture: Some(texture),
                    ..
                }) = self.attachment_previews.get(&file_id)
                {
                    let max_size = ui.available_size();
                    let mut size = texture.size_vec2();
                    let scale = (max_size.x / size.x).min(max_size.y / size.y).min(1.0);
                    size *= scale;
                    ui.add(egui::Image::new(texture).fit_to_exact_size(size));
                } else {
                    ui.label("Preview not available.");
                }
            });

        if !keep_open {
            self.expanded_preview = None;
        }
    }
}

fn human_readable_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes < KB {
        return format!("{bytes} B");
    }
    if bytes < MB {
        return format_scaled_unit(bytes, KB, "KB");
    }
    if bytes < GB {
        return format_scaled_unit(bytes, MB, "MB");
    }
    format_scaled_unit(bytes, GB, "GB")
}

fn format_scaled_unit(bytes: u64, unit_size: u64, unit_label: &str) -> String {
    let value = bytes as f64 / unit_size as f64;
    let value_text = format!("{value:.1}");
    let compact_value = value_text.strip_suffix(".0").unwrap_or(&value_text);
    format!("{compact_value} {unit_label}")
}

fn attachment_is_image(attachment: &AttachmentPayload) -> bool {
    attachment
        .mime_type
        .as_deref()
        .map(|mime| mime.starts_with("image/"))
        .unwrap_or_else(|| is_image_filename(&attachment.filename))
}

fn is_image_filename(filename: &str) -> bool {
    filename
        .rsplit('.')
        .next()
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff"
            )
        })
        .unwrap_or(false)
}

fn encode_rgba_png(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
        .ok_or_else(|| "invalid RGBA buffer".to_string())?;
    let dynamic = image::DynamicImage::ImageRgba8(image);
    let mut out = std::io::Cursor::new(Vec::new());
    dynamic
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|err| err.to_string())?;
    Ok(out.into_inner())
}

fn decode_image_for_clipboard(bytes: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let decoded = image::load_from_memory(bytes).map_err(|err| err.to_string())?;
    let rgba = decoded.to_rgba8();
    Ok((
        rgba.as_raw().to_vec(),
        rgba.width() as usize,
        rgba.height() as usize,
    ))
}

fn write_clipboard_image(rgba: &[u8], width: usize, height: usize) -> Result<(), String> {
    let mut clipboard = Clipboard::new().map_err(|err| err.to_string())?;
    clipboard
        .set_image(ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(rgba.to_vec()),
        })
        .map_err(|err| err.to_string())
}

fn decode_preview_image(bytes: &[u8]) -> Result<PreviewImage, String> {
    let dynamic = image::load_from_memory(bytes).map_err(|err| err.to_string())?;
    let resized = dynamic.thumbnail(1024, 1024).to_rgba8();
    let width = resized.width() as usize;
    let height = resized.height() as usize;
    Ok(PreviewImage {
        width,
        height,
        rgba: resized.into_raw(),
    })
}

fn queue_command(cmd_tx: &Sender<BackendCommand>, cmd: BackendCommand, status: &mut String) {
    let cmd_name = match &cmd {
        BackendCommand::Login { .. } => "login",
        BackendCommand::ListGuilds => "list_guilds",
        BackendCommand::ListChannels { .. } => "list_channels",
        BackendCommand::ListMembers { .. } => "list_members",
        BackendCommand::SelectChannel { .. } => "select_channel",
        BackendCommand::LoadMoreMessages { .. } => "load_more_messages",
        BackendCommand::SendMessage { .. } => "send_message",
        BackendCommand::DownloadAttachment { .. } => "download_attachment",
        BackendCommand::FetchAttachmentPreview { .. } => "fetch_attachment_preview",
        BackendCommand::CreateInvite { .. } => "create_invite",
        BackendCommand::JoinWithInvite { .. } => "join_with_invite",
        BackendCommand::ConnectVoice { .. } => "connect_voice",
        BackendCommand::DisconnectVoice => "disconnect_voice",
    };
    tracing::debug!(command = cmd_name, "queueing ui->backend command");
    match cmd_tx.try_send(cmd) {
        Ok(()) => {
            tracing::debug!(command = cmd_name, "queued ui->backend command");
        }
        Err(TrySendError::Full(_)) => {
            *status = "UI command queue is full; please retry".to_string();
            tracing::warn!(command = cmd_name, "ui->backend command queue is full");
        }
        Err(TrySendError::Disconnected(_)) => {
            let preserving_startup_error = cmd_name == "login"
                && status
                    .to_ascii_lowercase()
                    .contains("backend worker startup failure");
            if !preserving_startup_error {
                *status =
                    "Backend command processor disconnected (possible startup/runtime failure); retry sign-in"
                        .to_string();
            }
            tracing::error!(command = cmd_name, "ui->backend command queue disconnected");
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DiscordDarkPalette {
    app_background: egui::Color32,
    message_background: egui::Color32,
    members_background: egui::Color32,
    navigation_background: egui::Color32,
    guild_entry_hover: egui::Color32,
    guild_entry_active: egui::Color32,
    guild_entry_stroke: egui::Color32,
    guild_entry_stroke_active: egui::Color32,
    guild_text_unhighlighted: egui::Color32,
    guild_text_hovered: egui::Color32,
    guild_text_highlighted: egui::Color32,
    side_panel_button_fill: egui::Color32,
    side_panel_button_text: egui::Color32,
}

#[derive(Debug, Clone, Copy)]
struct MainWorkspaceLayout {
    guilds_panel_width: f32,
    channels_panel_width: f32,
    members_panel_width: f32,
    channel_row_height: f32,
    toolbar_h_padding: f32,
    toolbar_v_padding: f32,
    section_vertical_gap: f32,
    top_bar_h_padding: f32,
    top_bar_v_padding: f32,
}

impl Default for MainWorkspaceLayout {
    fn default() -> Self {
        Self {
            guilds_panel_width: 72.0,
            channels_panel_width: 240.0,
            members_panel_width: 240.0,
            channel_row_height: 34.0,
            toolbar_h_padding: 10.0,
            toolbar_v_padding: 8.0,
            section_vertical_gap: 8.0,
            top_bar_h_padding: 0.0,
            top_bar_v_padding: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MainWorkspaceColors {
    top_bar_bg: egui::Color32,
    nav_bg: egui::Color32,
    members_bg: egui::Color32,
    message_bg: egui::Color32,
}

#[derive(Debug, Clone, Copy)]
struct MainWorkspaceStyle {
    layout: MainWorkspaceLayout,
    colors: MainWorkspaceColors,
    discord_dark: Option<DiscordDarkPalette>,
}

fn theme_discord_dark_palette(theme: ThemeSettings) -> Option<DiscordDarkPalette> {
    (theme.preset == ThemePreset::DiscordDark).then_some({
        DiscordDarkPalette {
            app_background: egui::Color32::from_rgb(26, 26, 30),
            message_background: egui::Color32::from_rgb(26, 26, 30),
            members_background: egui::Color32::from_rgb(26, 26, 30),
            navigation_background: egui::Color32::from_rgb(18, 18, 20),
            guild_entry_hover: egui::Color32::from_rgb(29, 29, 30),
            guild_entry_active: egui::Color32::from_rgb(44, 44, 48),
            guild_entry_stroke: egui::Color32::from_rgb(48, 48, 56),
            guild_entry_stroke_active: egui::Color32::from_rgb(92, 92, 105),
            guild_text_unhighlighted: egui::Color32::from_rgb(129, 130, 138),
            guild_text_hovered: egui::Color32::from_rgb(251, 251, 251),
            guild_text_highlighted: egui::Color32::from_rgb(251, 251, 251),
            side_panel_button_fill: egui::Color32::from_rgb(35, 35, 40),
            side_panel_button_text: egui::Color32::from_rgb(236, 237, 240),
        }
    })
}

fn visuals_for_theme(theme: ThemeSettings) -> egui::Visuals {
    let mut visuals = match theme.preset {
        ThemePreset::DiscordDark => {
            let mut v = egui::Visuals::dark();
            let palette = theme_discord_dark_palette(theme)
                .expect("DiscordDark palette should exist for DiscordDark preset");
            v.override_text_color = Some(egui::Color32::from_rgb(251, 251, 251));
            v.window_fill = palette.app_background;
            v.panel_fill = palette.app_background;
            v.extreme_bg_color = palette.message_background;
            v.faint_bg_color = egui::Color32::from_rgb(29, 29, 30);
            v
        }
        ThemePreset::DiscordLegacy => {
            let mut v = egui::Visuals::dark();
            v.override_text_color = Some(egui::Color32::from_rgb(220, 221, 222));
            v.window_fill = egui::Color32::from_rgb(54, 57, 63);
            v.panel_fill = egui::Color32::from_rgb(47, 49, 54);
            v.extreme_bg_color = egui::Color32::from_rgb(32, 34, 37);
            v.faint_bg_color = egui::Color32::from_rgb(64, 68, 75);
            v
        }
        ThemePreset::AtomOneDark => {
            let mut v = egui::Visuals::dark();
            v.override_text_color = Some(egui::Color32::from_rgb(171, 178, 191));
            v.window_fill = egui::Color32::from_rgb(40, 44, 52);
            v.panel_fill = egui::Color32::from_rgb(33, 37, 43);
            v.extreme_bg_color = egui::Color32::from_rgb(24, 26, 31);
            v.faint_bg_color = egui::Color32::from_rgb(52, 57, 66);
            v
        }
        ThemePreset::EguiLight => egui::Visuals::light(),
    };

    visuals.hyperlink_color = theme.accent_color;
    visuals.selection.bg_fill = theme.accent_color;
    visuals.widgets.active.bg_fill = theme.accent_color;
    visuals.widgets.hovered.bg_fill = theme.accent_color.gamma_multiply(0.85);

    let radius = f32::from(theme.panel_rounding);
    visuals.widgets.noninteractive.rounding = egui::Rounding::same(radius);
    visuals.widgets.inactive.rounding = egui::Rounding::same(radius);
    visuals.widgets.hovered.rounding = egui::Rounding::same(radius);
    visuals.widgets.active.rounding = egui::Rounding::same(radius);
    visuals.widgets.open.rounding = egui::Rounding::same(radius);

    visuals
}

fn scaled_text_styles(text_scale: f32) -> BTreeMap<egui::TextStyle, egui::FontId> {
    let mut styles = egui::Style::default().text_styles;
    for font in styles.values_mut() {
        font.size *= text_scale;
    }
    styles
}

const SETTINGS_STORAGE_KEY: &str = "desktop_gui.settings";
const DESKTOP_GUI_DEVICE_ID: &str = "desktop-gui";

#[derive(Debug, Deserialize)]
struct LoginResponse {
    user_id: i64,
}

impl eframe::App for DesktopGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick = self.tick.wrapping_add(1);

        self.process_ui_events();
        self.apply_theme_if_needed(ctx);

        match self.view_state {
            AppViewState::Login => self.show_login_screen(ctx),
            AppViewState::Main => self.show_main_workspace(ctx),
        }

        if self.voice_ui.active_session.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = PersistedDesktopSettings::from_runtime(
            self.theme,
            self.readability,
            self.composer_panel_height,
            self.left_user_panel_height,
        );
        if let Ok(serialized) = serde_json::to_string(&settings) {
            storage.set_string(SETTINGS_STORAGE_KEY, serialized);
        }
    }
}

fn read_non_empty_env_var(name: &str, attempts: &mut Vec<String>) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => {
            attempts.push(format!("{name} was set but empty"));
            None
        }
        Ok(value) => Some(value),
        Err(err) => {
            attempts.push(format!("{name} unavailable: {err}"));
            None
        }
    }
}

fn resolve_mls_gui_data_dir() -> Result<PathBuf, String> {
    let mut attempts = Vec::new();

    if let Some(home) = read_non_empty_env_var("HOME", &mut attempts) {
        return Ok(PathBuf::from(home).join(".proto_rtc"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(userprofile) = read_non_empty_env_var("USERPROFILE", &mut attempts) {
            return Ok(PathBuf::from(userprofile).join(".proto_rtc"));
        }

        let homedrive = read_non_empty_env_var("HOMEDRIVE", &mut attempts);
        let homepath = read_non_empty_env_var("HOMEPATH", &mut attempts);
        match (homedrive, homepath) {
            (Some(homedrive), Some(homepath)) => {
                return Ok(PathBuf::from(format!("{homedrive}{homepath}")).join(".proto_rtc"));
            }
            _ => {
                attempts.push(
                    "HOMEDRIVE+HOMEPATH could not be used because one or both were unavailable"
                        .to_string(),
                );
            }
        }

        if let Some(local_app_data) = read_non_empty_env_var("LOCALAPPDATA", &mut attempts) {
            return Ok(PathBuf::from(local_app_data).join("proto_rtc"));
        }
    }

    Err(format!(
        "checked HOME{} and none provided a usable per-user directory ({})",
        if cfg!(target_os = "windows") {
            ", USERPROFILE, HOMEDRIVE+HOMEPATH, LOCALAPPDATA"
        } else {
            ""
        },
        attempts.join("; ")
    ))
}

fn resolve_user_mls_data_dir(base_dir: &std::path::Path, user_id: i64) -> PathBuf {
    base_dir.join("mls").join(format!("user_{user_id}"))
}

async fn fetch_user_id_for_login(server_url: &str, username: &str) -> Result<i64, String> {
    let response = HttpClient::new()
        .post(format!("{server_url}/login"))
        .json(&serde_json::json!({ "username": username }))
        .send()
        .await
        .map_err(|err| format!("failed to reach login endpoint: {err}"))?
        .error_for_status()
        .map_err(|err| format!("login endpoint returned error: {err}"))?;

    let body: LoginResponse = response
        .json()
        .await
        .map_err(|err| format!("invalid login response payload: {err}"))?;
    Ok(body.user_id)
}

async fn build_user_scoped_mls_client(
    base_dir: &std::path::Path,
    user_id: i64,
) -> Result<Arc<RealtimeClient<PassthroughCrypto>>, String> {
    let user_mls_state_dir = resolve_user_mls_data_dir(base_dir, user_id);
    std::fs::create_dir_all(&user_mls_state_dir).map_err(|err| {
        format!(
            "could not prepare per-user MLS state directory '{}' for user_id={user_id}: {err}",
            user_mls_state_dir.display()
        )
    })?;

    let mls_db_url = DurableMlsSessionManager::sqlite_url_for_gui_data_dir(&user_mls_state_dir);
    let mls_manager = DurableMlsSessionManager::initialize(&mls_db_url, user_id, DESKTOP_GUI_DEVICE_ID)
        .await
        .map_err(|err| {
            format!(
                "failed to initialize persistent MLS backend for user_id={user_id} ({mls_db_url}): {err:#}"
            )
        })?;

    Ok(RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        mls_manager,
    ))
}

fn spawn_backend_thread(cmd_rx: Receiver<BackendCommand>, ui_tx: Sender<UiEvent>) {
    thread::spawn(move || {
        let _ = ui_tx.try_send(UiEvent::Info("Backend worker starting...".to_string()));
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                    UiErrorContext::BackendStartup,
                    format!("backend worker startup failure: failed to build runtime: {err}"),
                )));
                tracing::error!("failed to build backend runtime: {err}");
                return;
            }
        };

        runtime.block_on(async move {
            let mls_state_dir = match resolve_mls_gui_data_dir() {
                Ok(path) => path,
                Err(attempted) => {
                    #[cfg(target_os = "windows")]
                    let guidance = "On Windows, set USERPROFILE (or LOCALAPPDATA) and relaunch the app.";
                    #[cfg(not(target_os = "windows"))]
                    let guidance = "Set HOME and relaunch the app.";

                    let user_message = format!(
                        "backend worker startup failure: could not resolve a writable MLS state directory. {guidance} Fallback resolution failed unexpectedly after trying: {attempted}."
                    );
                    let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                        UiErrorContext::BackendStartup,
                        user_message,
                    )));
                    tracing::error!(
                        "unable to initialize MLS state directory. {guidance} attempted strategy: {attempted}"
                    );
                    return;
                }
            };
            let attempted = if cfg!(target_os = "windows") {
                "HOME/.proto_rtc -> USERPROFILE/.proto_rtc -> HOMEDRIVE + HOMEPATH/.proto_rtc -> LOCALAPPDATA/proto_rtc"
            } else {
                "HOME/.proto_rtc"
            };
            if let Err(err) = std::fs::create_dir_all(&mls_state_dir) {
                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                    UiErrorContext::BackendStartup,
                    format!(
                        "backend worker startup failure: could not prepare MLS state directory '{}' (strategy: {attempted}). Ensure the configured user profile directory is writable and relaunch: {err}",
                        mls_state_dir.display()
                    ),
                )));
                tracing::error!(
                    "failed to create MLS state directory '{}' using strategy [{attempted}]. Ensure profile/app-data directory is writable: {err}",
                    mls_state_dir.display()
                );
                return;
            }

            let mut client = RealtimeClient::new(PassthroughCrypto);
            let _ = ui_tx.try_send(UiEvent::Info("Backend worker ready".to_string()));

            let mut event_task: Option<tokio::task::JoinHandle<()>> = None;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BackendCommand::Login {
                        server_url,
                        username,
                    } => {
                        let user_id = match fetch_user_id_for_login(&server_url, &username).await {
                            Ok(user_id) => user_id,
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::Login,
                                    err,
                                )));
                                continue;
                            }
                        };

                        let rebound_client = match build_user_scoped_mls_client(&mls_state_dir, user_id).await {
                            Ok(client) => client,
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::Login,
                                    err,
                                )));
                                continue;
                            }
                        };

                        if let Some(task) = event_task.take() {
                            task.abort();
                        }

                        let mut events = rebound_client.subscribe_events();
                        let ui_tx_clone = ui_tx.clone();
                        event_task = Some(tokio::spawn(async move {
                            while let Ok(event) = events.recv().await {
                                let evt = match event {
                                    ClientEvent::Server(evt) => UiEvent::Server(evt),
                                    ClientEvent::UserDirectoryUpdated { user_id, username } => {
                                        UiEvent::SenderDirectoryUpdated { user_id, username }
                                    }
                                    ClientEvent::MessageDecrypted { message, plaintext } => {
                                        UiEvent::MessageDecrypted { message, plaintext }
                                    }
                                    ClientEvent::VoiceSessionStateChanged(snapshot) => {
                                        UiEvent::VoiceSessionStateChanged(snapshot)
                                    }
                                    ClientEvent::VoiceParticipantsUpdated {
                                        guild_id,
                                        channel_id,
                                        participants,
                                    } => UiEvent::VoiceParticipantsUpdated {
                                        guild_id,
                                        channel_id,
                                        participants,
                                    },
                                    ClientEvent::Error(err) => UiEvent::Error(
                                        UiError::from_message(UiErrorContext::DecryptMessage, err),
                                    ),
                                };
                                let _ = ui_tx_clone.try_send(evt);
                            }
                        }));

                        client = rebound_client;
                        match client.login(&server_url, &username, "").await {
                            Ok(()) => {
                                let _ = ui_tx.try_send(UiEvent::LoginOk);
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::Login,
                                    err.to_string(),
                                )));
                            }
                        }
                    }
                    BackendCommand::ListGuilds => {
                        if let Err(err) = client.list_guilds().await {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::ListChannels { guild_id } => {
                        if let Err(err) = client.list_channels(guild_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::ListMembers { guild_id } => {
                        if let Err(err) = client.list_members(guild_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::SelectChannel { channel_id } => {
                        if let Err(err) = client.select_channel(channel_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::LoadMoreMessages { channel_id, before } => {
                        if let Err(err) = client.fetch_messages(channel_id, 100, Some(before)).await
                        {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::SendMessage {
                        text,
                        attachment_path,
                    } => {
                        let result = if let Some(path) = attachment_path {
                            let filename = path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("attachment.bin")
                                .to_string();
                            match tokio::fs::read(&path).await {
                                Ok(bytes) => {
                                    let mime_type = mime_guess::from_path(&path)
                                        .first_raw()
                                        .map(str::to_string);
                                    client
                                        .send_message_with_attachment(
                                            &text,
                                            AttachmentUpload {
                                                filename,
                                                mime_type,
                                                ciphertext: bytes,
                                            },
                                        )
                                        .await
                                }
                                Err(err) => Err(err.into()),
                            }
                        } else {
                            client.send_message(&text).await
                        };

                        if let Err(err) = result {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::SendMessage,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::FetchAttachmentPreview { file_id } => {
                        match client.download_file(file_id).await {
                            Ok(bytes) => {
                                // `bytes` might be Vec<u8>, bytes::Bytes, or something slice-like.
                                // Make an owned Vec<u8> once, use it for both decode + storage.
                                let slice: &[u8] = bytes.as_ref();
                                let owned: Vec<u8> = slice.to_vec();

                                match decode_preview_image(&owned) {
                                    Ok(image) => {
                                        let _ = ui_tx.try_send(UiEvent::AttachmentPreviewLoaded {
                                            file_id,
                                            image,
                                            original_bytes: owned,
                                        });
                                    }
                                    Err(err) => {
                                        let _ = ui_tx.try_send(UiEvent::AttachmentPreviewFailed {
                                            file_id,
                                            reason: err,
                                        });
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::AttachmentPreviewFailed {
                                    file_id,
                                    reason: format!("Failed to download preview: {err}"),
                                });
                            }
                        }
                    }
                    BackendCommand::DownloadAttachment { file_id, filename } => {
                        match client.download_file(file_id).await {
                            Ok(bytes) => {
                                let save_path =
                                    rfd::FileDialog::new().set_file_name(&filename).save_file();
                                if let Some(path) = save_path {
                                    match tokio::fs::write(&path, bytes).await {
                                        Ok(()) => {
                                            let _ = ui_tx.try_send(UiEvent::Info(format!(
                                                "Saved attachment to {}",
                                                path.display()
                                            )));
                                        }
                                        Err(err) => {
                                            let _ = ui_tx.try_send(UiEvent::Error(
                                                UiError::from_message(
                                                    UiErrorContext::General,
                                                    format!("Failed to save attachment: {err}"),
                                                ),
                                            ));
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::General,
                                    format!("Failed to download attachment: {err}"),
                                )));
                            }
                        }
                    }
                    BackendCommand::CreateInvite { guild_id } => {
                        match client.create_invite(guild_id).await {
                            Ok(invite_code) => {
                                let _ = ui_tx.try_send(UiEvent::InviteCreated(invite_code));
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::General,
                                    err.to_string(),
                                )));
                            }
                        }
                    }
                    BackendCommand::JoinWithInvite { invite_code } => {
                        match client.join_with_invite(&invite_code).await {
                            Ok(()) => {
                                let _ = ui_tx.try_send(UiEvent::Info(
                                    "Joined guild from invite".to_string(),
                                ));
                                if let Err(err) = client.list_guilds().await {
                                    let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                        UiErrorContext::General,
                                        err.to_string(),
                                    )));
                                }
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                    UiErrorContext::General,
                                    err.to_string(),
                                )));
                            }
                        }
                    }
                    BackendCommand::ConnectVoice { guild_id, channel_id } => {
                        if let Err(err) = client
                            .connect_voice_session(VoiceConnectOptions {
                                guild_id,
                                channel_id,
                                can_publish_mic: true,
                                can_publish_screen: true,
                            })
                            .await
                        {
                            let _ = ui_tx.try_send(UiEvent::VoiceOperationFailed(err.to_string()));
                        }
                    }
                    BackendCommand::DisconnectVoice => {
                        if let Err(err) = client.disconnect_voice_session().await {
                            let _ = ui_tx.try_send(UiEvent::VoiceOperationFailed(err.to_string()));
                        }
                    }
                }
            }
        });
    });
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let (cmd_tx, cmd_rx) = bounded::<BackendCommand>(256);
    let (ui_tx, ui_rx) = bounded::<UiEvent>(2048);
    spawn_backend_thread(cmd_rx, ui_tx);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Prototype RTC Desktop GUI")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Prototype RTC Desktop GUI",
        options,
        Box::new(|cc| {
            let persisted_settings = cc.storage.and_then(|storage| {
                storage
                    .get_string(SETTINGS_STORAGE_KEY)
                    .and_then(|text| serde_json::from_str::<PersistedDesktopSettings>(&text).ok())
            });
            Ok(Box::new(DesktopGuiApp::new(
                cmd_tx,
                ui_rx,
                persisted_settings,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::{human_readable_bytes, DisplayMessage, UiError, UiErrorCategory, UiErrorContext};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use shared::domain::{ChannelId, MessageId, UserId};
    use shared::protocol::MessagePayload;

    #[test]
    fn formats_attachment_sizes_readably() {
        assert_eq!(human_readable_bytes(0), "0 B");
        assert_eq!(human_readable_bytes(1023), "1023 B");
        assert_eq!(human_readable_bytes(1024), "1 KB");
        assert_eq!(human_readable_bytes(1536), "1.5 KB");
        assert_eq!(human_readable_bytes(2 * 1024 * 1024), "2 MB");
        assert_eq!(human_readable_bytes(1572864), "1.5 MB");
        assert_eq!(human_readable_bytes(3 * 1024 * 1024 * 1024), "3 GB");
    }

    #[test]
    fn classifies_backend_command_processor_disconnect_as_transport_error() {
        let err = UiError::from_message(
            UiErrorContext::General,
            "Backend command processor disconnected (possible startup/runtime failure)",
        );
        assert_eq!(err.category, UiErrorCategory::Transport);
        assert!(!err.requires_reauth());
    }

    #[test]
    fn display_message_uses_decrypted_plaintext_for_rendering() {
        let message = DisplayMessage {
            wire: MessagePayload {
                message_id: MessageId(1),
                channel_id: ChannelId(9),
                sender_id: UserId(42),
                sender_username: Some("alice".to_string()),
                ciphertext_b64: STANDARD.encode(b"ciphertext"),
                attachment: None,
                sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
            },
            plaintext: "hello from mls".to_string(),
        };

        assert_eq!(message.plaintext, "hello from mls");
    }
}
