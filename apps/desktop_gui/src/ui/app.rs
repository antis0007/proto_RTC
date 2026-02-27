use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    thread,
    time::SystemTime,
};

use arboard::{Clipboard, ImageData};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Local;

fn ui_in_rect(ui: &mut egui::Ui, rect: egui::Rect, add: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    add(&mut child);
}

use crate::backend_bridge::commands::BackendCommand;
use crate::controller::events::{
    classify_login_failure, UiError, UiErrorCategory, UiErrorContext, UiEvent,
};
use crate::controller::orchestration::dispatch_backend_command;
use client_core::{
    AttachmentUpload, ClientEvent, ClientHandle, DurableMlsSessionManager, PassthroughCrypto,
    RealtimeClient, VoiceConnectOptions, VoiceParticipantState, VoiceSessionSnapshot,
};
use crossbeam_channel::{Receiver, Sender};
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

#[derive(Debug, Clone)]
pub struct StartupConfig {
    pub server_url: String,
    pub username: String,
    pub display_name: String,
    pub data_dir: Option<std::path::PathBuf>,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:8443".to_string(),
            username: "alice".to_string(),
            display_name: "alice".to_string(),
            data_dir: None,
        }
    }
}
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_root: std::path::PathBuf, // per-profile isolated
    pub cache_dir: std::path::PathBuf,
    pub db_path: std::path::PathBuf,
    pub mls_dir: std::path::PathBuf,
    pub settings_path: std::path::PathBuf,
}

impl AppPaths {
    pub fn from_startup(startup: &StartupConfig) -> anyhow::Result<Self> {
        let root = if let Some(p) = &startup.data_dir {
            p.clone()
        } else {
            // Fallback: per-user app data + username namespace
            let base = dirs::data_local_dir()
                .ok_or_else(|| anyhow::anyhow!("unable to resolve local app data dir"))?;
            base.join("proto_rtc")
                .join("profiles")
                .join(&startup.username)
        };

        Ok(Self {
            cache_dir: root.join("cache"),
            db_path: root.join("client.sqlite3"),
            mls_dir: root.join("mls"),
            settings_path: root.join("settings.json"),
            data_root: root,
        })
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
pub(crate) struct PreviewImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}
#[derive(Clone)]
pub(crate) struct AnimatedGifFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    delay_ms: u32,
}

#[derive(Clone)]
pub(crate) struct DecodedGifPreview {
    frames: Vec<AnimatedGifFrame>,
}

struct GifPlaybackPreview {
    frames: Vec<AnimatedGifFrame>,
    current_frame: usize,
    next_frame_at_secs: f64,
    texture: Option<egui::TextureHandle>,
}
enum GifAnimationState {
    NotRequested,
    Loading,
    Ready(GifPlaybackPreview),
    Error(String),
}
enum AttachmentPreviewState {
    NotRequested,
    Loading,
    Ready {
        image: PreviewImage,
        original_bytes: Vec<u8>,
        preview_png: Option<Vec<u8>>,
        texture: Option<egui::TextureHandle>,
        gif_animation: Option<GifPlaybackPreview>,
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
    show_message_time_timezone: bool,
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
            show_message_time_timezone: true,
        }
    }
}

impl PersistedDesktopSettings {
    fn into_runtime(self) -> (ThemeSettings, UiReadabilitySettings, f32, f32, bool) {
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
            self.show_message_time_timezone, // <- add this
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
            show_message_time_timezone: true,
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

pub struct DesktopGuiApp {
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
    attachment_menu_open: bool,
    attachment_menu_anchor: Option<egui::Rect>,
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
    account_menu_open: bool,
    view_state: AppViewState,

    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
    readability: UiReadabilitySettings,
    applied_readability: Option<UiReadabilitySettings>,
    show_message_time_timezone: bool,
    composer_panel_height: f32,
    left_user_panel_height: f32,
    left_sidebar_width: f32,
    members_panel_width: f32,
    guilds_column_width: f32,
    show_members_panel: bool,
    show_guilds_panel: bool,
    history: Vec<String>,
    history_index: usize,

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
    pub fn bootstrap(
        cmd_tx: Sender<BackendCommand>,
        ui_rx: Receiver<UiEvent>,
        startup: StartupConfig,
    ) -> Self {
        Self::new(cmd_tx, ui_rx, None, startup)
    }

    fn new(
        cmd_tx: Sender<BackendCommand>,
        ui_rx: Receiver<UiEvent>,
        persisted_settings: Option<PersistedDesktopSettings>,
        startup: StartupConfig,
    ) -> Self {
        let (
            theme,
            readability,
            composer_panel_height,
            left_user_panel_height,
            show_message_time_timezone,
        ) = persisted_settings.unwrap_or_default().into_runtime();
        Self {
            cmd_tx,
            ui_rx,
            server_url: startup.server_url.clone(),
            username: startup.username.clone(),
            invite_code_input: String::new(),
            auth_session_established: false,
            presence_preference: AccountPresence::Online,
            notifications_enabled: true,
            desktop_notifications_enabled: true,
            mention_notifications_enabled: true,
            display_name_draft: startup.display_name.clone(),
            composer: String::new(),
            pending_attachment: None,
            attachment_menu_open: false,
            attachment_menu_anchor: None,
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
            account_menu_open: false,
            view_state: AppViewState::Login,
            theme,
            applied_theme: None,
            readability,
            applied_readability: None,
            composer_panel_height,
            left_user_panel_height,
            left_sidebar_width: 360.0,
            members_panel_width: 250.0,
            guilds_column_width: 118.0,
            show_members_panel: true,
            show_guilds_panel: true,
            history: vec!["#general".to_string()],
            history_index: 0,
            login_ui: LoginUiState::default(),
            tick: 0,
            show_message_time_timezone,
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
                    dispatch_backend_command(
                        &self.cmd_tx,
                        BackendCommand::ListGuilds,
                        &mut self.status,
                    );
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
                UiEvent::JoinedGuild(guild_id) => {
                    self.selected_guild = Some(guild_id);
                    self.selected_channel = None;
                    self.channels.clear();
                    dispatch_backend_command(
                        &self.cmd_tx,
                        BackendCommand::ListChannels { guild_id },
                        &mut self.status,
                    );
                    dispatch_backend_command(
                        &self.cmd_tx,
                        BackendCommand::ListMembers { guild_id },
                        &mut self.status,
                    );
                }
                UiEvent::SenderDirectoryUpdated { user_id, username } => {
                    self.sender_directory.insert(user_id, username);
                }
                UiEvent::Error(err) => {
                    if err.requires_reauth() {
                        self.auth_session_established = false;
                        self.view_state = AppViewState::Login;
                        self.status = format!("Authentication error: {}", err.message());
                        self.status_banner = Some(StatusBanner {
                            severity: StatusBannerSeverity::Error,
                            message:
                                "Session expired or invalid credentials. Please sign in again."
                                    .to_string(),
                        });
                        self.login_ui.focus = Some(LoginFocusField::Username);
                    } else {
                        self.status = if err.context() == UiErrorContext::Login {
                            classify_login_failure(&err.message())
                        } else {
                            format!("{} error: {}", err_label(err.category()), err.message())
                        };
                        if matches!(
                            err.context(),
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
                    decoded_gif,
                } => {
                    let preview_png = encode_rgba_png(&image.rgba, image.width, image.height).ok();
                    self.attachment_previews.insert(
                        file_id,
                        AttachmentPreviewState::Ready {
                            image,
                            original_bytes,
                            preview_png,
                            texture: None,
                            gif_animation: decoded_gif.map(|g| GifPlaybackPreview {
                                frames: g.frames,
                                current_frame: 0,
                                next_frame_at_secs: 0.0,
                                texture: None,
                            }),
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
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::ListChannels { guild_id },
                                &mut self.status,
                            );
                            dispatch_backend_command(
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
                                dispatch_backend_command(
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

        let window_frame = egui::Frame::NONE
            .fill(ctx.style().visuals.window_fill)
            .stroke(egui::Stroke::new(
                1.0,
                ctx.style().visuals.window_stroke().color,
            ))
            .corner_radius(self.popup_corner_radius())
            .inner_margin(egui::Margin::symmetric(12, 10));

        let mut settings_open = self.settings_open;
        let mut close_requested = false;

        egui::Window::new("settings_window")
            .title_bar(false)
            .frame(window_frame)
            .open(&mut settings_open)
            .resizable(false)
            .show(ctx, |ui| {
                self.apply_popup_menu_style(ui);
                ui.horizontal(|ui| {
                    self.show_popup_section_title(ui, "Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            close_requested = true;
                        }
                    });
                });
                ui.separator();
                self.show_popup_section_title(ui, "Theme");
                ui.label("Theme preset");
                egui::ComboBox::from_id_salt("theme_preset")
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
                self.show_popup_section_title(ui, "Colors");
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
                self.show_popup_section_title(ui, "Readability");
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

        self.settings_open = settings_open && !close_requested;
    }

    fn show_status_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(banner) = self.status_banner.clone() {
            let (fill, stroke) = match banner.severity {
                StatusBannerSeverity::Error => (
                    egui::Color32::from_rgb(111, 53, 53),
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(175, 96, 96)),
                ),
            };

            egui::Frame::NONE
                .fill(fill)
                .stroke(stroke)
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
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

    fn popup_corner_radius(&self) -> egui::CornerRadius {
        egui::CornerRadius::same(self.theme.panel_rounding)
    }

    fn apply_popup_menu_style(&self, ui: &mut egui::Ui) {
        let s = ui.style_mut();
        let radius = self.popup_corner_radius();
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.visuals.widgets.inactive.corner_radius = radius;
        s.visuals.widgets.hovered.corner_radius = radius;
        s.visuals.widgets.active.corner_radius = radius;
        s.visuals.widgets.open.corner_radius = radius;
        s.visuals.widgets.noninteractive.corner_radius = radius;
    }

    fn show_popup_section_title(&self, ui: &mut egui::Ui, title: &str) {
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(13.0 * self.readability.text_scale),
        );
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
            .id_salt(id)
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

                egui::Frame::NONE
                    .fill(card_fill)
                    .corner_radius(14.0)
                    .stroke(egui::Stroke::new(
                        1.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    ))
                    .inner_margin(egui::Margin::symmetric(20, 18))
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
                        egui::Frame::NONE
                            .fill(ui.visuals().faint_bg_color.gamma_multiply(0.55))
                            .corner_radius(12.0)
                            .inner_margin(egui::Margin::symmetric(14, 12))
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
                                    .stroke(egui::Stroke::new(1.0, p.nav_item_stroke_active));
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
        dispatch_backend_command(
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

        let is_gif = bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a");
        let decoded = if is_gif {
            match decode_first_gif_frame_image(&bytes) {
                Ok(image) => image,
                Err(_) => {
                    self.attachment_preview_cache
                        .insert(cache_key, AttachmentPreview::DecodeFailed);
                    return Some(AttachmentPreview::DecodeFailed);
                }
            }
        } else {
            match image::load_from_memory(&bytes) {
                Ok(image) => image,
                Err(_) => {
                    self.attachment_preview_cache
                        .insert(cache_key, AttachmentPreview::DecodeFailed);
                    return Some(AttachmentPreview::DecodeFailed);
                }
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
            dispatch_backend_command(
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
        self.apply_popup_menu_style(ui);
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
            ui.close();
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
            ui.close();
        }

        let save_full = ui
            .add_enabled(full_bytes.is_some(), egui::Button::new("Save image as…"))
            .on_hover_text("Saves original bytes when available.")
            .clicked();
        if save_full {
            if let Some(bytes) = full_bytes {
                self.save_image_bytes_as(bytes, file_name);
            }
            ui.close();
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
            ui.close();
        }

        ui.separator();
        if ui.button("Copy file name").clicked() {
            ui.ctx().copy_text(file_name.to_string());
            self.status = "Copied file name to clipboard".to_string();
            ui.close();
        }

        if ui
            .add_enabled(metadata.is_some(), egui::Button::new("Copy metadata"))
            .clicked()
        {
            if let Some(text) = metadata {
                ui.ctx().copy_text(text.to_string());
                self.status = "Copied image metadata to clipboard".to_string();
            }
            ui.close();
        }
    }

    #[allow(dead_code)]
    fn render_left_user_panel(&mut self, ui: &mut egui::Ui) {
        let palette =
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette);

        egui::Frame::new()
            .fill(egui::Color32::TRANSPARENT)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                let content_h = 54.0;
                let extra = (ui.available_height() - content_h).max(0.0);
                ui.add_space(extra * 0.5);

                // Resolve selected voice channel info without holding long borrows.
                let selected_voice = self.selected_voice_channel().cloned().or_else(|| {
                    self.voice_ui.active_session.as_ref().and_then(|active| {
                        self.channels
                            .iter()
                            .find(|c| c.channel_id == active.channel_id)
                            .cloned()
                    })
                });

                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let (avatar, _) =
                            ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
                        ui.painter().rect_filled(
                            avatar,
                            egui::CornerRadius::same(16),
                            egui::Color32::from_rgb(70, 75, 90),
                        );
                        ui.painter().circle_filled(
                            egui::pos2(avatar.right() - 4.0, avatar.bottom() - 4.0),
                            4.0,
                            egui::Color32::from_rgb(35, 165, 90),
                        );

                        ui.add_space(4.0);

                        ui.vertical(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.username)
                                        .color(palette.title_text)
                                        .strong()
                                        .size(13.0 * self.readability.text_scale),
                                )
                                .selectable(false),
                            );

                            let status = if self.voice_ui.deafened {
                                "Deafened"
                            } else if self.voice_ui.muted {
                                "Muted"
                            } else if self.voice_ui.active_session.is_some() {
                                "In Voice"
                            } else {
                                "Online"
                            };

                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(status)
                                        .color(palette.message_hint_text)
                                        .size(11.0 * self.readability.text_scale),
                                )
                                .selectable(false),
                            );
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(icon_btn("⚙", egui::Color32::from_rgb(185, 187, 190), None))
                                .clicked()
                            {
                                self.settings_open = true;
                            }

                            let share_on = self.voice_ui.screen_share_enabled;
                            if ui
                                .add(icon_btn(
                                    "🖥",
                                    if share_on {
                                        egui::Color32::from_rgb(88, 101, 242)
                                    } else {
                                        egui::Color32::from_rgb(185, 187, 190)
                                    },
                                    if share_on {
                                        Some(egui::Color32::from_rgb(37, 45, 86))
                                    } else {
                                        None
                                    },
                                ))
                                .on_hover_text("Screen share (UI state)")
                                .clicked()
                            {
                                // Backend toggle command not yet exposed in BackendCommand; preserve UI state for now.
                                self.voice_ui.screen_share_enabled =
                                    !self.voice_ui.screen_share_enabled;
                            }

                            let deafen_on = self.voice_ui.deafened;
                            if ui
                                .add(icon_btn(
                                    "🎧",
                                    if deafen_on {
                                        egui::Color32::from_rgb(240, 71, 71)
                                    } else {
                                        egui::Color32::from_rgb(185, 187, 190)
                                    },
                                    if deafen_on {
                                        Some(egui::Color32::from_rgb(63, 39, 43))
                                    } else {
                                        None
                                    },
                                ))
                                .clicked()
                            {
                                self.voice_ui.deafened = !self.voice_ui.deafened;
                                if self.voice_ui.deafened {
                                    self.voice_ui.muted = true;
                                }
                            }

                            let mute_on = self.voice_ui.muted;
                            if ui
                                .add(icon_btn(
                                    "🎤",
                                    if mute_on {
                                        egui::Color32::from_rgb(240, 71, 71)
                                    } else {
                                        egui::Color32::from_rgb(185, 187, 190)
                                    },
                                    if mute_on {
                                        Some(egui::Color32::from_rgb(63, 39, 43))
                                    } else {
                                        None
                                    },
                                ))
                                .clicked()
                            {
                                self.voice_ui.muted = !self.voice_ui.muted;
                                if !self.voice_ui.muted {
                                    self.voice_ui.deafened = false;
                                }
                            }
                        });
                    });

                    ui.add_space(6.0);

                    // Mockup-inspired voice strip with actual join/leave behavior.
                    egui::Frame::new()
                        .fill(palette.nav_item_hover)
                        .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(6, 4))
                        .show(ui, |ui| {
                            let active_name =
                                self.voice_ui.active_session.as_ref().and_then(|active| {
                                    self.channels
                                        .iter()
                                        .find(|c| c.channel_id == active.channel_id)
                                        .map(|c| c.name.clone())
                                });

                            let selected_name = selected_voice.as_ref().map(|c| c.name.clone());
                            let channel_name = active_name
                                .or(selected_name)
                                .unwrap_or_else(|| "No voice channel selected".to_string());

                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("🔊")
                                        .color(palette.nav_text)
                                        .size(13.0 * self.readability.text_scale),
                                );

                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(channel_name)
                                            .color(palette.nav_text_hover)
                                            .size(12.0 * self.readability.text_scale),
                                    );

                                    let status_text = if self.voice_ui.active_session.is_some() {
                                        format!(
                                            "{} participant(s)",
                                            self.voice_ui.active_participant_count()
                                        )
                                    } else {
                                        "Not connected".to_string()
                                    };

                                    ui.label(
                                        egui::RichText::new(status_text)
                                            .color(palette.message_hint_text)
                                            .size(10.5 * self.readability.text_scale),
                                    );
                                });

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let can_act = selected_voice.is_some();
                                        let connected_here = selected_voice
                                            .as_ref()
                                            .map(|ch| {
                                                self.voice_ui.is_channel_connected(ch.channel_id)
                                            })
                                            .unwrap_or(false);

                                        let label = if connected_here { "Leave" } else { "Join" };

                                        let mut btn = egui::Button::new(
                                            egui::RichText::new(label)
                                                .size(11.0 * self.readability.text_scale)
                                                .color(palette.title_text),
                                        )
                                        .min_size(egui::vec2(56.0, 22.0))
                                        .corner_radius(egui::CornerRadius::same(4));

                                        btn = if connected_here {
                                            btn.fill(egui::Color32::from_rgb(63, 39, 43)).stroke(
                                                egui::Stroke::new(
                                                    1.0,
                                                    egui::Color32::from_rgb(240, 71, 71),
                                                ),
                                            )
                                        } else {
                                            btn.fill(palette.nav_item_active).stroke(
                                                egui::Stroke::new(
                                                    1.0,
                                                    palette.nav_item_stroke_active,
                                                ),
                                            )
                                        };

                                        if ui.add_enabled(can_act, btn).clicked() {
                                            if let Some(ch) = selected_voice.clone() {
                                                self.queue_voice_join_leave(
                                                    ch.guild_id,
                                                    ch.channel_id,
                                                );
                                            }
                                        }
                                    },
                                );
                            });
                        });
                });
            });
    }

    // ------------------- Main workspace (mostly unchanged) -------------------

    fn show_account_menu_contents(&mut self, ui: &mut egui::Ui) {
        self.apply_popup_menu_style(ui);
        let auth_ready = self.auth_session_established;
        let backend_supports_profile_edit = false;

        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 1.0);

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
            ui.close();
        }

        ui.separator();
        ui.small("Presence");
        egui::ComboBox::from_id_salt("account_presence_combo")
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
            ui.close();
        }
    }

    fn refresh_selected_guild_navigation(&mut self) {
        if let Some(guild_id) = self.selected_guild {
            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::ListChannels { guild_id },
                &mut self.status,
            );
            dispatch_backend_command(
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

            let (label_color, fill, stroke) = if let Some(palette) = discord_dark {
                (
                    palette.nav_text,
                    palette.nav_background,
                    egui::Stroke::new(1.0, palette.nav_item_stroke),
                )
            } else {
                (
                    ui.visuals().text_color(),
                    ui.visuals().widgets.inactive.bg_fill,
                    ui.visuals().widgets.inactive.bg_stroke,
                )
            };

            let mut refresh_button =
                egui::Button::new(egui::RichText::new("Refresh").color(label_color))
                    .fill(fill)
                    .stroke(stroke)
                    .min_size(egui::vec2(68.0, 22.0));

            if let Some(palette) = discord_dark {
                refresh_button = refresh_button.corner_radius(egui::CornerRadius::same(4));
                let resp = ui.add(refresh_button);
                if resp.hovered() {
                    ui.painter().rect_filled(
                        resp.rect,
                        egui::CornerRadius::same(4),
                        palette.nav_item_hover,
                    );
                    ui.painter().rect_stroke(
                        resp.rect,
                        egui::CornerRadius::same(4),
                        egui::Stroke::new(1.0, palette.nav_item_stroke_active),
                        egui::StrokeKind::Middle,
                    );
                    ui.painter().text(
                        resp.rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Refresh",
                        egui::FontId::proportional(13.0),
                        palette.nav_text_hover,
                    );
                }
                if resp.clicked() {
                    self.refresh_selected_guild_navigation();
                }
            } else if ui.add(refresh_button).clicked() {
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
                egui::RichText::new(cta_label).color(palette.nav_text)
            } else {
                egui::RichText::new(cta_label)
            };

            let cta_button = self
                .sidebar_button(cta_text, discord_dark)
                .min_size(egui::vec2(ui.available_width(), 30.0));

            if ui.add(cta_button).clicked() {
                self.queue_voice_join_leave(channel.guild_id, channel.channel_id);
            }
        }
    }
    fn queue_voice_join_leave(&mut self, guild_id: GuildId, channel_id: ChannelId) {
        let connected_here = self.voice_ui.is_channel_connected(channel_id);

        if connected_here {
            self.voice_ui.connection_status = VoiceSessionConnectionStatus::Disconnecting;
            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::DisconnectVoice,
                &mut self.status,
            );
        } else {
            self.voice_ui.connection_status = VoiceSessionConnectionStatus::Connecting;
            self.voice_ui.last_error = None;
            self.voice_ui.selected_voice_channel = Some(channel_id);

            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::ConnectVoice {
                    guild_id,
                    channel_id,
                },
                &mut self.status,
            );
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
                .map(|p| p.nav_background)
                .unwrap_or(ctx.style().visuals.panel_fill),
            nav_bg: discord_dark
                .map(|p| p.nav_background)
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
                .fill(palette.nav_background)
                .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke));
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
        let base_bg = discord_dark.map(|p| p.nav_background).unwrap_or_else(|| {
            if self.theme.list_row_shading {
                ui.visuals().faint_bg_color
            } else {
                egui::Color32::TRANSPARENT
            }
        });
        let selected_bg = discord_dark.map(|p| p.nav_item_active).unwrap_or_else(|| {
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
                        palette.nav_item_stroke_active
                    } else {
                        palette.nav_item_stroke
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
                .map(|p| p.nav_item_hover)
                .unwrap_or_else(|| ui.visuals().widgets.hovered.bg_fill)
        } else {
            base_bg
        };

        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(u8::from(self.theme.panel_rounding)),
            row_fill,
        );
        if row_stroke != egui::Stroke::NONE {
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(u8::from(self.theme.panel_rounding)),
                row_stroke,
                egui::StrokeKind::Middle,
            );
        }

        let text_color = if selected {
            discord_dark
                .map(|p| p.nav_text_highlighted)
                .unwrap_or(ui.visuals().strong_text_color())
        } else if response.hovered() {
            discord_dark
                .map(|p| p.nav_text_hover)
                .unwrap_or(ui.visuals().strong_text_color())
        } else {
            discord_dark
                .map(|p| p.nav_text)
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
            .frame(egui::Frame::NONE.fill(style.colors.nav_bg).inner_margin(
                egui::Margin::symmetric(
                    style.layout.toolbar_h_padding as i8,
                    style.layout.toolbar_v_padding as i8,
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
                        let top_region_height = ui.max_rect().height();

                        ui.allocate_ui_with_layout(
                            egui::vec2(guilds_width, top_region_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.heading("Guilds");
                                ui.add_space(style.layout.section_vertical_gap);

                                egui::ScrollArea::vertical()
                                    .id_salt("left_nav_guilds_scroll")
                                    .auto_shrink([false, false])
                                    .max_height(ui.available_height())
                                    .show(ui, |ui| {
                                        ui.add_enabled_ui(self.auth_session_established, |ui| {
                                            if let Some(guild_id) = self.selected_guild {
                                                let label = if let Some(palette) = discord_dark {
                                                    egui::RichText::new("Create Invite")
                                                        .color(palette.nav_text)
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
                                                    dispatch_backend_command(
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
                                                    dispatch_backend_command(
                                                        &self.cmd_tx,
                                                        BackendCommand::ListChannels {
                                                            guild_id: guild.guild_id,
                                                        },
                                                        &mut self.status,
                                                    );
                                                    dispatch_backend_command(
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

                        let channels_width =
                            (ui.available_width() - ui.spacing().item_spacing.x).max(0.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(channels_width, top_region_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                self.show_channels_panel_header(ui, discord_dark);
                                ui.add_space(style.layout.section_vertical_gap);

                                egui::ScrollArea::vertical()
                                    .id_salt("left_nav_channels_scroll")
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

    fn show_guilds_panel_header(
        &mut self,
        ui: &mut egui::Ui,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        let text = if let Some(palette) = discord_dark {
            egui::RichText::new("SERVERS")
                .size(11.0)
                .strong()
                .color(palette.message_hint_text)
        } else {
            egui::RichText::new("SERVERS").size(11.0).strong()
        };
        ui.label(text);
    }

    fn render_guild_row(
        &mut self,
        ui: &mut egui::Ui,
        guild_id: GuildId,
        guild_name: &str,
        row_height: f32,
        discord_dark: Option<DiscordDarkPalette>,
    ) -> egui::Response {
        let selected = self.selected_guild == Some(guild_id);
        let response = self.render_nav_row(ui, guild_name, row_height, selected, discord_dark);
        if response.clicked() {
            self.selected_guild = Some(guild_id);
            self.selected_channel = None;
            self.channels.clear();
            self.members.remove(&guild_id);
            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::ListChannels { guild_id },
                &mut self.status,
            );
            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::ListMembers { guild_id },
                &mut self.status,
            );
        }
        response
    }

    fn show_guilds_list(&mut self, ui: &mut egui::Ui, discord_dark: Option<DiscordDarkPalette>) {
        egui::Frame::new()
            .fill(egui::Color32::TRANSPARENT)
            .inner_margin(egui::Margin::symmetric(8, 8))
            .show(ui, |ui| {
                self.show_guilds_panel_header(ui, discord_dark);
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .id_salt("guilds_split_list_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for index in 0..self.guilds.len() {
                            let guild_id = self.guilds[index].guild_id;
                            let guild_name = self.guilds[index].name.clone();
                            self.render_guild_row(ui, guild_id, &guild_name, 42.0, discord_dark);
                        }
                    });
            });
    }

    fn show_channels_list(&mut self, ui: &mut egui::Ui, style: MainWorkspaceStyle) {
        let discord_dark = style.discord_dark;

        egui::Frame::new()
            .fill(egui::Color32::TRANSPARENT)
            .inner_margin(egui::Margin::symmetric(8, 0)) // left/right breathing room
            .show(ui, |ui| {
                self.show_channels_panel_header(ui, discord_dark);
                ui.add_space(style.layout.section_vertical_gap);

                egui::ScrollArea::vertical()
                    .id_salt("channels_split_list_scroll")
                    .auto_shrink([false, false])
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
        let palette = discord_dark.unwrap_or_else(|| {
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette)
        });

        let is_selected = self.selected_channel == Some(channel_id);
        let is_voice = kind == ChannelKind::Voice;
        let is_connected_here = is_voice && self.voice_ui.is_channel_connected(channel_id);
        let occupancy = if is_voice {
            Some(self.voice_ui.occupancy_for_channel(channel_id))
        } else {
            None
        };

        let desired = egui::vec2(ui.available_width(), row_height.max(30.0));
        let (row_rect, row_resp) = ui.allocate_exact_size(desired, egui::Sense::click());

        let hovered = ui.rect_contains_pointer(row_rect);

        let row_fill = if is_selected {
            palette.nav_item_active
        } else if hovered {
            palette.nav_item_hover
        } else {
            egui::Color32::TRANSPARENT
        };

        if row_fill != egui::Color32::TRANSPARENT {
            ui.painter()
                .rect_filled(row_rect, egui::CornerRadius::same(4), row_fill);
        }

        // Selection marker (mockup-inspired)
        if is_selected {
            let marker = egui::Rect::from_min_max(
                egui::pos2(row_rect.left() + 2.0, row_rect.top() + 5.0),
                egui::pos2(row_rect.left() + 5.0, row_rect.bottom() - 5.0),
            );
            ui.painter().rect_filled(
                marker,
                egui::CornerRadius::same(2),
                palette.nav_text_highlighted,
            );
        }

        // Inner layout region
        let inner = row_rect.shrink2(egui::vec2(8.0, 4.0));
        ui_in_rect(ui, inner, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.spacing_mut().item_spacing.y = 0.0;

            ui.horizontal(|ui| {
                let icon = match kind {
                    ChannelKind::Text => "#",
                    ChannelKind::Voice => "🔊",
                };

                let text_color = if is_selected {
                    palette.nav_text_highlighted
                } else if hovered {
                    palette.nav_text_hover
                } else {
                    palette.nav_text
                };

                ui.label(
                    egui::RichText::new(icon)
                        .color(text_color)
                        .size(14.0 * self.readability.text_scale),
                );

                ui.label(
                    egui::RichText::new(name)
                        .color(text_color)
                        .size(13.0 * self.readability.text_scale),
                );

                if let Some(count) = occupancy {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("{count}"))
                            .color(palette.message_hint_text)
                            .size(11.0 * self.readability.text_scale),
                    );
                }

                // Mockup-inspired inline voice action button on the right
                if is_voice {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_label = if is_connected_here { "Leave" } else { "Join" };

                        let mut btn = egui::Button::new(
                            egui::RichText::new(btn_label)
                                .size(11.0 * self.readability.text_scale)
                                .color(if is_connected_here {
                                    palette.title_text
                                } else {
                                    palette.nav_text_hover
                                }),
                        )
                        .corner_radius(egui::CornerRadius::same(4))
                        .min_size(egui::vec2(52.0, (row_height - 8.0).clamp(20.0, 24.0)));

                        btn =
                            if is_connected_here {
                                btn.fill(egui::Color32::from_rgb(63, 39, 43)).stroke(
                                    egui::Stroke::new(1.0, egui::Color32::from_rgb(240, 71, 71)),
                                )
                            } else {
                                btn.fill(palette.nav_item_active)
                                    .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke_active))
                            };

                        let join_resp = ui.add(btn);

                        if join_resp.clicked() {
                            // Need guild id to connect; derive from current channel list entry safely.
                            if let Some(ch) = self
                                .channels
                                .iter()
                                .find(|c| c.channel_id == channel_id)
                                .cloned()
                            {
                                self.queue_voice_join_leave(ch.guild_id, ch.channel_id);
                            }
                        }
                    });
                }
            });
        });

        // Whole row click still selects channel (without breaking the inline button behavior)
        if row_resp.clicked() {
            self.selected_channel = Some(channel_id);
            if is_voice {
                self.voice_ui.selected_voice_channel = Some(channel_id);
            }
            dispatch_backend_command(
                &self.cmd_tx,
                BackendCommand::SelectChannel { channel_id },
                &mut self.status,
            );
        }

        ui.add_space(4.0);
    }

    fn apply_topbar_scope_style(&self, ui: &mut egui::Ui) {
        let s = ui.style_mut();
        s.spacing.item_spacing = egui::vec2(4.0, 0.0);
        s.spacing.button_padding = egui::vec2(8.0, 2.0);
        s.spacing.interact_size.y = 22.0;

        // Flat, squared controls to match the mockup top bars.
        s.visuals.widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
        s.visuals.widgets.hovered.corner_radius = egui::CornerRadius::ZERO;
        s.visuals.widgets.active.corner_radius = egui::CornerRadius::ZERO;
        s.visuals.widgets.open.corner_radius = egui::CornerRadius::ZERO;
    }

    fn topbar_flat_button(
        &self,
        label: impl Into<egui::WidgetText>,
        fill: egui::Color32,
        stroke: egui::Color32,
    ) -> egui::Button<'static> {
        egui::Button::new(label)
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(egui::CornerRadius::ZERO)
    }

    fn show_members_side_panel(&mut self, ctx: &egui::Context, style: MainWorkspaceStyle) {
        let palette =
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette);
        egui::SidePanel::right("members_panel")
            .default_width(style.layout.members_panel_width)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(style.colors.members_bg)
                    .inner_margin(egui::Margin::symmetric(
                        style.layout.toolbar_h_padding as i8,
                        style.layout.toolbar_v_padding as i8,
                    ))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.heading("Members");
                            if ui.button("Refresh").clicked() {
                                if let Some(guild_id) = self.selected_guild {
                                    dispatch_backend_command(
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

                        let selected_voice = self.selected_voice_channel().cloned();

                        if let Some(channel) = selected_voice.as_ref() {
                            ui.label(format!("Selected: {}", channel.name));
                            ui.small(format!(
                                "Occupancy: {} participant(s)",
                                self.voice_ui.occupancy_for_channel(channel.channel_id)
                            ));

                            let connected_here =
                                self.voice_ui.is_channel_connected(channel.channel_id);
                            let btn_label = if connected_here {
                                "Leave voice"
                            } else {
                                "Join voice"
                            };

                            if ui.button(btn_label).clicked() {
                                self.queue_voice_join_leave(channel.guild_id, channel.channel_id);
                            }
                        }

                        if let Some(active) = &self.voice_ui.active_session {
                            let active_channel_id = active.channel_id;
                            let active_name = self
                                .channels
                                .iter()
                                .find(|channel| channel.channel_id == active_channel_id)
                                .map(|channel| channel.name.as_str())
                                .unwrap_or("Unknown voice channel");

                            ui.add_space(6.0);
                            ui.label(format!("Connected channel: {active_name}"));
                            ui.label(format!(
                                "Participants: {}",
                                self.voice_ui.active_participant_count()
                            ));

                            ui.horizontal_wrapped(|ui| {
                                if ui.toggle_value(&mut self.voice_ui.muted, "Mute").changed()
                                    && !self.voice_ui.muted
                                {
                                    self.voice_ui.deafened = false;
                                }
                                if ui
                                    .toggle_value(&mut self.voice_ui.deafened, "Deafen")
                                    .changed()
                                    && self.voice_ui.deafened
                                {
                                    self.voice_ui.muted = true;
                                }
                                ui.toggle_value(
                                    &mut self.voice_ui.screen_share_enabled,
                                    "Screen Share",
                                );
                            });

                            if let Some(participants) = self
                                .voice_ui
                                .participants_by_channel
                                .get(&active_channel_id)
                            {
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new("In call")
                                        .size(11.0)
                                        .strong()
                                        .color(palette.message_hint_text),
                                );

                                for p in participants {
                                    let name = if !p.identity.trim().is_empty() {
                                        p.identity.clone()
                                    } else {
                                        format!("Participant {}", p.participant_id)
                                    };

                                    let mut suffix = String::new();
                                    if p.muted {
                                        suffix.push_str(" · 🔇");
                                    }
                                    if p.deafened {
                                        suffix.push_str(" · 🎧");
                                    }

                                    ui.label(
                                        egui::RichText::new(format!("• {name}{suffix}"))
                                            .color(palette.message_text)
                                            .size(12.0 * self.readability.text_scale),
                                    );
                                }
                            }
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

    fn show_top_bar(&mut self, ctx: &egui::Context, _style: MainWorkspaceStyle) {
        let palette = theme_discord_dark_palette(self.theme).unwrap_or(DiscordDarkPalette {
            app_background: egui::Color32::from_rgb(26, 26, 30),
            nav_background: egui::Color32::from_rgb(18, 18, 20),
            message_background: egui::Color32::from_rgb(26, 26, 30),
            message_hover: egui::Color32::from_rgb(36, 36, 40),
            members_background: egui::Color32::from_rgb(26, 26, 30),
            members_hover: egui::Color32::from_rgb(43, 45, 49),
            nav_text: egui::Color32::from_rgb(170, 171, 179),
            nav_text_hover: egui::Color32::WHITE,
            nav_text_highlighted: egui::Color32::WHITE,
            message_text: egui::Color32::WHITE,
            message_hint_text: egui::Color32::from_rgb(108, 109, 118),
            title_text: egui::Color32::WHITE,
            nav_title_text: egui::Color32::WHITE,
            nav_item_hover: egui::Color32::from_rgb(29, 29, 30),
            nav_item_active: egui::Color32::from_rgb(44, 44, 48),
            nav_item_stroke: egui::Color32::from_rgb(48, 48, 56),
            nav_item_stroke_active: egui::Color32::from_rgb(92, 92, 105),
        });

        let top_fill = egui::Color32::from_rgb(19, 20, 24);
        let sub_fill = egui::Color32::from_rgb(24, 25, 30);
        let btn_fill = egui::Color32::from_rgb(34, 35, 40);
        let btn_stroke = egui::Color32::from_rgb(58, 60, 70);

        egui::TopBottomPanel::top("app_top_menu_bar")
            .resizable(false)
            .exact_height(26.0)
            .frame(
                egui::Frame::new()
                    .fill(top_fill)
                    .inner_margin(egui::Margin {
                        top: 1,
                        bottom: 0,
                        left: 6,
                        right: 6,
                    }),
            )
            .show(ctx, |ui| {
                self.apply_topbar_scope_style(ui);

                ui.horizontal(|ui| {
                    if ui
                        .add(
                            self.topbar_flat_button(
                                egui::RichText::new("Settings").color(palette.nav_text_hover),
                                btn_fill,
                                btn_stroke,
                            )
                            .min_size(egui::vec2(70.0, 22.0)),
                        )
                        .clicked()
                    {
                        self.settings_open = !self.settings_open;
                    }

                    let account_label = if self.account_menu_open {
                        "Account ▾"
                    } else {
                        "Account"
                    };
                    if ui.button(account_label).clicked() {
                        self.account_menu_open = !self.account_menu_open;
                    }

                    ui.menu_button("View", |ui| {
                        self.apply_popup_menu_style(ui);
                        self.apply_topbar_scope_style(ui);
                        ui.checkbox(&mut self.show_members_panel, "Members panel");
                        ui.checkbox(&mut self.show_guilds_panel, "Guilds column");
                    });

                    ui.menu_button("Help", |ui| {
                        self.apply_popup_menu_style(ui);
                        self.apply_topbar_scope_style(ui);
                        ui.label("Proto RTC desktop client");
                        ui.small("Discord-style UI (egui)");
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let env = server_environment_label(&self.server_url);
                        ui.label(
                            egui::RichText::new(env)
                                .color(palette.message_hint_text)
                                .size(11.0),
                        );
                    });
                });
            });

        if self.account_menu_open {
            let mut keep_open = true;
            egui::Window::new("Account")
                .id(egui::Id::new("account_menu_window"))
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::LEFT_TOP, egui::vec2(78.0, 28.0))
                .open(&mut keep_open)
                .frame(
                    egui::Frame::popup(&ctx.style())
                        .fill(palette.app_background)
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(58, 60, 70)))
                        .corner_radius(egui::CornerRadius::same(6)),
                )
                .show(ctx, |ui| {
                    self.apply_topbar_scope_style(ui);
                    self.show_account_menu_contents(ui);
                });
            self.account_menu_open = keep_open;
        }

        egui::TopBottomPanel::top("sub_top_bar")
            .resizable(false)
            .exact_height(34.0)
            .frame(
                egui::Frame::new()
                    .fill(sub_fill)
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ctx, |ui| {
                self.apply_topbar_scope_style(ui);
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.horizontal(|ui| {
                    let can_back = self.history_index > 0;
                    let back_btn = self
                        .topbar_flat_button(
                            egui::RichText::new("◀").color(if can_back {
                                palette.nav_text_hover
                            } else {
                                palette.message_hint_text
                            }),
                            btn_fill,
                            btn_stroke,
                        )
                        .min_size(egui::vec2(24.0, 22.0));
                    if ui.add_enabled(can_back, back_btn).clicked() {
                        self.history_index = self.history_index.saturating_sub(1);
                    }

                    let can_fwd = self.history_index + 1 < self.history.len();
                    let fwd_btn = self
                        .topbar_flat_button(
                            egui::RichText::new("▶").color(if can_fwd {
                                palette.nav_text_hover
                            } else {
                                palette.message_hint_text
                            }),
                            btn_fill,
                            btn_stroke,
                        )
                        .min_size(egui::vec2(24.0, 22.0));
                    if ui.add_enabled(can_fwd, fwd_btn).clicked() {
                        self.history_index =
                            (self.history_index + 1).min(self.history.len().saturating_sub(1));
                    }

                    let refresh_btn = self
                        .topbar_flat_button(
                            egui::RichText::new("⟳").color(palette.nav_text_hover),
                            btn_fill,
                            btn_stroke,
                        )
                        .min_size(egui::vec2(24.0, 22.0));
                    if ui.add(refresh_btn).clicked() {
                        if let Some(guild_id) = self.selected_guild {
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::ListChannels { guild_id },
                                &mut self.status,
                            );
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::ListMembers { guild_id },
                                &mut self.status,
                            );
                        }
                        dispatch_backend_command(
                            &self.cmd_tx,
                            BackendCommand::ListGuilds,
                            &mut self.status,
                        );
                    }

                    ui.separator();

                    if ui
                        .add(
                            self.topbar_flat_button(
                                egui::RichText::new("Create Invite").color(palette.nav_text_hover),
                                btn_fill,
                                btn_stroke,
                            )
                            .min_size(egui::vec2(88.0, 22.0)),
                        )
                        .clicked()
                    {
                        if let Some(guild_id) = self.selected_guild {
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::CreateInvite { guild_id },
                                &mut self.status,
                            );
                        } else {
                            self.status = "Select a server first".to_string();
                        }
                    }

                    let join_w = 170.0_f32.min((ui.available_width() * 0.28).max(100.0));
                    let edit_resp = ui.add_sized(
                        [join_w, 22.0],
                        egui::TextEdit::singleline(&mut self.invite_code_input)
                            .id_salt("top_invite_code")
                            .hint_text(
                                egui::RichText::new("Invite code").color(palette.message_hint_text),
                            ),
                    );

                    if ui
                        .add(
                            self.topbar_flat_button(
                                egui::RichText::new("Join").color(palette.nav_text_hover),
                                btn_fill,
                                btn_stroke,
                            )
                            .min_size(egui::vec2(44.0, 22.0)),
                        )
                        .clicked()
                    {
                        let code = self.invite_code_input.trim().to_string();
                        if code.is_empty() {
                            self.status = "Enter an invite code first".to_string();
                        } else {
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::JoinWithInvite { invite_code: code },
                                &mut self.status,
                            );
                        }
                    }

                    if edit_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let code = self.invite_code_input.trim().to_string();
                        if !code.is_empty() {
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::JoinWithInvite { invite_code: code },
                                &mut self.status,
                            );
                        }
                    }

                    ui.separator();

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                self.topbar_flat_button(
                                    egui::RichText::new("Refresh Guilds")
                                        .color(palette.nav_text_hover),
                                    btn_fill,
                                    btn_stroke,
                                )
                                .min_size(egui::vec2(96.0, 22.0)),
                            )
                            .clicked()
                        {
                            dispatch_backend_command(
                                &self.cmd_tx,
                                BackendCommand::ListGuilds,
                                &mut self.status,
                            );
                        }
                    });
                });
            });
    }

    fn show_main_workspace(&mut self, ctx: &egui::Context) {
        let style = self.main_workspace_style(ctx);
        self.show_top_bar(ctx, style);
        self.show_settings_window(ctx);
        // Members panel is split manually (mockup-style) for reliable resize hit-testing/cursor feedback.

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(style.colors.message_bg)
                    .inner_margin(egui::Margin::same(0)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();

                // First split: message area vs members panel (right), mockup-style manual splitter.
                let min_members = 180.0;
                let max_members = 420.0;
                let min_root_center = 260.0;
                let root_splitter_w = if self.show_members_panel { 8.0 } else { 0.0 };

                let mut content_rect = full;
                let mut members_rect_opt: Option<egui::Rect> = None;
                let mut members_split_x_opt: Option<f32> = None;
                let mut members_split_resp_opt: Option<egui::Response> = None;

                if self.show_members_panel {
                    self.members_panel_width =
                        self.members_panel_width.clamp(min_members, max_members);
                    let max_members_now = (full.width() - min_root_center)
                        .max(min_members)
                        .min(max_members);
                    self.members_panel_width = self.members_panel_width.min(max_members_now);

                    let mut members_split_x = (full.right() - self.members_panel_width)
                        .clamp(full.left() + min_root_center, full.right() - min_members);

                    let members_hit_rect = egui::Rect::from_min_max(
                        egui::pos2(members_split_x - 6.0, full.top()),
                        egui::pos2(members_split_x + 6.0, full.bottom()),
                    );
                    let members_resp = ui.interact(
                        members_hit_rect,
                        ui.make_persistent_id("members_manual_splitter"),
                        egui::Sense::click_and_drag(),
                    );

                    if members_resp.dragged() {
                        if let Some(pointer) = members_resp.interact_pointer_pos() {
                            members_split_x = pointer
                                .x
                                .clamp(full.left() + min_root_center, full.right() - min_members);
                            self.members_panel_width = (full.right() - members_split_x)
                                .clamp(min_members, max_members_now);
                            ui.ctx().request_repaint();
                        }
                    }

                    members_split_x = (full.right() - self.members_panel_width)
                        .clamp(full.left() + min_root_center, full.right() - min_members);

                    content_rect = egui::Rect::from_min_max(
                        full.min,
                        egui::pos2(members_split_x - root_splitter_w * 0.5, full.bottom()),
                    );
                    members_rect_opt = Some(egui::Rect::from_min_max(
                        egui::pos2(members_split_x + root_splitter_w * 0.5, full.top()),
                        full.max,
                    ));
                    members_split_x_opt = Some(members_split_x);
                    members_split_resp_opt = Some(members_resp);
                }

                let full = content_rect;
                let min_left = 240.0;
                let min_center = 220.0;
                let splitter_w = 8.0;
                let max_left = (full.width() - min_center).max(min_left).min(560.0);
                self.left_sidebar_width = self.left_sidebar_width.clamp(min_left, max_left);
                let mut split_x = (full.left() + self.left_sidebar_width)
                    .clamp(full.left() + min_left, full.right() - min_center);

                let splitter_rect = egui::Rect::from_min_max(
                    egui::pos2(split_x - 6.0, full.top()),
                    egui::pos2(split_x + 6.0, full.bottom()),
                );
                let resp = ui.interact(
                    splitter_rect,
                    ui.make_persistent_id("left_nav_manual_splitter"),
                    egui::Sense::click_and_drag(),
                );
                if resp.dragged() {
                    if let Some(pointer) = resp.interact_pointer_pos() {
                        split_x = pointer
                            .x
                            .clamp(full.left() + min_left, full.right() - min_center);
                        self.left_sidebar_width = (split_x - full.left()).clamp(min_left, max_left);
                    }
                }

                let nav_rect = egui::Rect::from_min_max(
                    full.min,
                    egui::pos2(split_x - splitter_w * 0.5, full.bottom()),
                );
                let center_rect = egui::Rect::from_min_max(
                    egui::pos2(split_x + splitter_w * 0.5, full.top()),
                    full.max,
                );

                ui_in_rect(ui, nav_rect, |ui| {
                    self.show_nav_sidebar_area(ui, style.discord_dark);
                });

                ui_in_rect(ui, center_rect, |ui| {
                    self.show_message_area(ui, style.discord_dark);
                });

                ui.painter().line_segment(
                    [
                        egui::pos2(split_x, full.top()),
                        egui::pos2(split_x, full.bottom()),
                    ],
                    egui::Stroke::new(
                        1.0,
                        if resp.hovered() || resp.dragged() {
                            style
                                .discord_dark
                                .map(|p| p.nav_item_stroke_active)
                                .unwrap_or(style.colors.nav_bg)
                        } else {
                            egui::Color32::from_rgb(40, 41, 46)
                        },
                    ),
                );
                if resp.hovered() || resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }

                if let (Some(members_rect), Some(members_split_x), Some(members_resp)) = (
                    members_rect_opt,
                    members_split_x_opt,
                    members_split_resp_opt,
                ) {
                    ui_in_rect(ui, members_rect, |ui| {
                        egui::Frame::new()
                            .fill(style.colors.members_bg)
                            .inner_margin(egui::Margin::same(0))
                            .show(ui, |ui| {
                                self.show_members_side_panel_contents(ui, style.discord_dark)
                            });
                    });

                    ui.painter().line_segment(
                        [
                            egui::pos2(members_split_x, members_rect.top()),
                            egui::pos2(members_split_x, members_rect.bottom()),
                        ],
                        egui::Stroke::new(
                            1.0,
                            if members_resp.hovered() || members_resp.dragged() {
                                style
                                    .discord_dark
                                    .map(|p| p.nav_item_stroke_active)
                                    .unwrap_or(style.colors.nav_bg)
                            } else {
                                egui::Color32::from_rgb(40, 41, 46)
                            },
                        ),
                    );

                    if members_resp.hovered() || members_resp.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    }
                }
            });

        self.render_expanded_preview_window(ctx);
    }

    fn show_nav_sidebar_area(
        &mut self,
        ui: &mut egui::Ui,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        let palette = discord_dark.unwrap_or_else(|| {
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette)
        });
        let full = ui.max_rect();

        let min_user_h = 48.0;
        let max_user_h = 140.0;
        let min_top_h = 120.0;
        let splitter_h = 8.0;

        self.left_user_panel_height = self.left_user_panel_height.clamp(min_user_h, max_user_h);

        let mut split_y = (full.bottom() - self.left_user_panel_height)
            .clamp(full.top() + min_top_h, full.bottom() - min_user_h);

        let splitter_rect = egui::Rect::from_min_max(
            egui::pos2(full.left(), split_y - splitter_h * 0.5),
            egui::pos2(full.right(), split_y + splitter_h * 0.5),
        );

        let splitter_id = ui.make_persistent_id("nav_user_info_splitter_manual");
        let splitter_resp = ui.interact(splitter_rect, splitter_id, egui::Sense::click_and_drag());

        if splitter_resp.dragged() {
            if let Some(pointer) = splitter_resp.interact_pointer_pos() {
                split_y = pointer
                    .y
                    .clamp(full.top() + min_top_h, full.bottom() - min_user_h);

                self.left_user_panel_height =
                    (full.bottom() - split_y).clamp(min_user_h, max_user_h);
                ui.ctx().request_repaint();
            }
        }

        split_y = (full.bottom() - self.left_user_panel_height)
            .clamp(full.top() + min_top_h, full.bottom() - min_user_h);

        let top_rect = egui::Rect::from_min_max(
            full.min,
            egui::pos2(full.right(), split_y - splitter_h * 0.5),
        );
        let bottom_rect = egui::Rect::from_min_max(
            egui::pos2(full.left(), split_y + splitter_h * 0.5),
            full.max,
        );

        ui_in_rect(ui, top_rect, |ui| {
            let full = ui.max_rect();

            if self.show_guilds_panel {
                let min_guilds = 96.0;
                let min_channels = 120.0;
                let splitter_w = 8.0;

                let max_guilds = (full.width() - min_channels).max(min_guilds).min(240.0);
                self.guilds_column_width = self.guilds_column_width.clamp(min_guilds, max_guilds);

                let mut guilds_split_x = (full.left() + self.guilds_column_width)
                    .clamp(full.left() + min_guilds, full.right() - min_channels);

                let guilds_splitter_rect = egui::Rect::from_min_max(
                    egui::pos2(guilds_split_x - 6.0, full.top()),
                    egui::pos2(guilds_split_x + 6.0, full.bottom()),
                );
                let guilds_split_resp = ui.interact(
                    guilds_splitter_rect,
                    ui.make_persistent_id("guilds_channels_manual_splitter"),
                    egui::Sense::click_and_drag(),
                );

                if guilds_split_resp.dragged() {
                    if let Some(pointer) = guilds_split_resp.interact_pointer_pos() {
                        guilds_split_x = pointer
                            .x
                            .clamp(full.left() + min_guilds, full.right() - min_channels);
                        self.guilds_column_width =
                            (guilds_split_x - full.left()).clamp(min_guilds, max_guilds);
                        ui.ctx().request_repaint();
                    }
                }

                guilds_split_x = (full.left() + self.guilds_column_width)
                    .clamp(full.left() + min_guilds, full.right() - min_channels);

                let guilds_rect = egui::Rect::from_min_max(
                    full.min,
                    egui::pos2(guilds_split_x - splitter_w * 0.5, full.bottom()),
                );
                let channels_rect = egui::Rect::from_min_max(
                    egui::pos2(guilds_split_x + splitter_w * 0.5, full.top()),
                    full.max,
                );

                ui_in_rect(ui, guilds_rect, |ui| {
                    egui::Frame::new()
                        .fill(self.main_workspace_style(ui.ctx()).colors.nav_bg)
                        .inner_margin(egui::Margin::same(0))
                        .show(ui, |ui| {
                            self.show_guilds_list(ui, Some(palette));
                        });
                });

                ui_in_rect(ui, channels_rect, |ui| {
                    egui::Frame::new()
                        .fill(self.main_workspace_style(ui.ctx()).colors.nav_bg)
                        .inner_margin(egui::Margin::same(0))
                        .show(ui, |ui| {
                            self.show_channels_list(ui, self.main_workspace_style(ui.ctx()));
                        });
                });

                ui.painter().line_segment(
                    [
                        egui::pos2(guilds_split_x, full.top()),
                        egui::pos2(guilds_split_x, full.bottom()),
                    ],
                    egui::Stroke::new(
                        1.0,
                        if guilds_split_resp.hovered() || guilds_split_resp.dragged() {
                            palette.nav_item_stroke_active
                        } else {
                            egui::Color32::from_rgb(40, 41, 46)
                        },
                    ),
                );

                if guilds_split_resp.hovered() || guilds_split_resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
            } else {
                egui::Frame::new()
                    .fill(self.main_workspace_style(ui.ctx()).colors.nav_bg)
                    .inner_margin(egui::Margin::same(0))
                    .show(ui, |ui| {
                        self.show_channels_list(ui, self.main_workspace_style(ui.ctx()));
                    });
            }
        });
        ui_in_rect(ui, bottom_rect, |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(22, 22, 24),
            );

            egui::Frame::new()
                .fill(egui::Color32::TRANSPARENT)
                .inner_margin(egui::Margin::same(0))
                .show(ui, |ui| {
                    self.render_left_user_panel(ui);
                });
        });

        let stroke_col = egui::Color32::from_rgb(40, 41, 46);
        ui.painter().line_segment(
            [
                egui::pos2(full.left(), split_y),
                egui::pos2(full.right(), split_y),
            ],
            egui::Stroke::new(
                1.0,
                if splitter_resp.hovered() || splitter_resp.dragged() {
                    palette.nav_item_stroke_active
                } else {
                    stroke_col
                },
            ),
        );

        let stroke_col = egui::Color32::from_rgb(40, 41, 46);
        ui.painter().line_segment(
            [
                egui::pos2(bottom_rect.left(), bottom_rect.top()),
                egui::pos2(bottom_rect.right(), bottom_rect.top()),
            ],
            egui::Stroke::new(1.0, stroke_col),
        );
        ui.painter().line_segment(
            [
                egui::pos2(bottom_rect.left(), bottom_rect.top()),
                egui::pos2(bottom_rect.left(), bottom_rect.bottom()),
            ],
            egui::Stroke::new(1.0, stroke_col),
        );
        ui.painter().line_segment(
            [
                egui::pos2(bottom_rect.right(), bottom_rect.top()),
                egui::pos2(bottom_rect.right(), bottom_rect.bottom()),
            ],
            egui::Stroke::new(1.0, stroke_col),
        );
        ui.painter().line_segment(
            [
                egui::pos2(top_rect.right(), top_rect.top()),
                egui::pos2(top_rect.right(), top_rect.bottom()),
            ],
            egui::Stroke::new(1.0, stroke_col),
        );

        if splitter_resp.hovered() || splitter_resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
    }
    fn render_mock_message_attachment_row(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &AttachmentPayload,
        starts_block: bool,
        palette: DiscordDarkPalette,
    ) {
        let scale = self.readability.text_scale.clamp(0.85, 2.0);

        let pad_x = 10.0 * scale;
        let text_gap_x = 8.0 * scale;
        let top_pad = 4.0 * scale;
        let bottom_pad = 10.0 * scale; // extra breathing room so buttons never clip

        let avatar_size = (if self.readability.compact_density {
            28.0
        } else {
            36.0
        }) * scale;
        let avatar_slot_w = avatar_size + 8.0 * scale;
        let row_w = ui.available_width();

        let left_indent = pad_x + avatar_slot_w + text_gap_x;
        let right_pad = pad_x;
        let content_w = (row_w - left_indent - right_pad).max(80.0);

        // Give non-image attachments plenty of room for:
        // filename line + compact button + spacing
        let min_non_image_h = 82.0 * scale;

        // Safe over-estimate to avoid hover/background clipping through button text.
        let estimated_inner_h = if attachment_is_image(attachment) {
            match self.attachment_previews.get(&attachment.file_id) {
                Some(AttachmentPreviewState::Ready { image, .. }) => {
                    let max_width = (content_w * 0.75).clamp(120.0, 380.0);
                    let preview_w = image.width as f32;
                    let preview_h = image.height as f32;

                    let scaled_h = if preview_w > 0.0 && preview_h > 0.0 {
                        let mut w = preview_w;
                        let mut h = preview_h;
                        if w > max_width {
                            let k = max_width / w;
                            w *= k;
                            h *= k;
                        }
                        h.min(260.0)
                    } else {
                        48.0 * scale
                    };

                    // preview + file line + button row + extra bottom spacing
                    scaled_h + (84.0 * scale)
                }
                Some(AttachmentPreviewState::Error(_)) => 60.0 * scale,
                Some(AttachmentPreviewState::Loading)
                | Some(AttachmentPreviewState::NotRequested)
                | None => 42.0 * scale,
            }
        } else {
            min_non_image_h
        };

        let row_h = estimated_inner_h + top_pad + bottom_pad;
        let (row_rect, _) = ui.allocate_exact_size(egui::vec2(row_w, row_h), egui::Sense::hover());
        let row_clip = row_rect.intersect(ui.clip_rect());

        if ui.rect_contains_pointer(row_rect) {
            ui.painter().with_clip_rect(row_clip).rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                palette.message_hover,
            );
        }

        let content_rect = egui::Rect::from_min_max(
            egui::pos2(row_rect.left() + left_indent, row_rect.top() + top_pad),
            egui::pos2(row_rect.right() - right_pad, row_rect.bottom() - bottom_pad),
        );

        let attach_rect = if starts_block {
            content_rect.translate(egui::vec2(0.0, 0.5 * scale))
        } else {
            content_rect
        };

        ui_in_rect(ui, attach_rect, |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(row_clip));
            ui.set_width(content_w);
            ui.set_max_width(content_w);
            ui.spacing_mut().item_spacing.y = 6.0 * scale;

            if attachment_is_image(attachment) {
                self.render_image_attachment_preview(ui, attachment, palette);
            } else {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(
                        palette.message_hover.r(),
                        palette.message_hover.g(),
                        palette.message_hover.b(),
                        24,
                    ))
                    .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "📎 {} ({})",
                                    attachment.filename,
                                    human_readable_bytes(attachment.size_bytes)
                                ))
                                .color(palette.message_text),
                            );
                        });

                        self.render_attachment_download_row(ui, attachment, palette);
                    });
            }
        });
    }
    fn show_members_side_panel_contents(
        &mut self,
        ui: &mut egui::Ui,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        let palette = discord_dark.unwrap_or_else(|| {
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette)
        });
        egui::Frame::new()
            .fill(palette.members_background)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(guild_id) = self.selected_guild {
                        if let Some(members) = self.members.get(&guild_id) {
                            let online = members.iter().filter(|m| !m.muted).count();
                            ui.label(
                                egui::RichText::new(format!("ONLINE — {online}"))
                                    .size(11.0)
                                    .color(palette.message_hint_text)
                                    .strong(),
                            );
                            for m in members.iter().filter(|m| !m.muted) {
                                self.render_member_row(ui, m, true, Some(palette));
                            }
                            ui.add_space(10.0);
                            let offline = members.iter().filter(|m| m.muted).count();
                            ui.label(
                                egui::RichText::new(format!("OFFLINE — {offline}"))
                                    .size(11.0)
                                    .color(palette.message_hint_text)
                                    .strong(),
                            );
                            for m in members.iter().filter(|m| m.muted) {
                                self.render_member_row(ui, m, false, Some(palette));
                            }
                        }
                    }
                });
            });
    }

    fn render_member_row(
        &self,
        ui: &mut egui::Ui,
        m: &MemberSummary,
        online: bool,
        discord_dark: Option<DiscordDarkPalette>,
    ) {
        let palette = discord_dark.unwrap_or_else(|| {
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette)
        });
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
        let hovered = ui.rect_contains_pointer(rect);
        if hovered {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), palette.members_hover);
        }
        let row = rect.shrink2(egui::vec2(6.0, 4.0));
        let avatar_rect = egui::Rect::from_min_size(
            egui::pos2(row.left(), row.center().y - 14.0),
            egui::vec2(28.0, 28.0),
        );
        ui.painter().rect_filled(
            avatar_rect,
            egui::CornerRadius::same(14),
            if online {
                egui::Color32::from_rgb(76, 91, 135)
            } else {
                egui::Color32::from_rgb(72, 72, 75)
            },
        );
        ui.painter().circle_filled(
            egui::pos2(avatar_rect.right() - 3.5, avatar_rect.bottom() - 3.5),
            3.5,
            if online {
                egui::Color32::from_rgb(35, 165, 90)
            } else {
                egui::Color32::from_rgb(116, 127, 141)
            },
        );
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(avatar_rect.right() + 8.0, row.top()),
            egui::pos2(row.right(), row.bottom()),
        );
        ui_in_rect(ui, text_rect, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.label(
                egui::RichText::new(&m.username)
                    .size(13.0)
                    .color(if online {
                        palette.nav_text_hover
                    } else {
                        palette.nav_text
                    }),
            );
            let status = if online {
                match m.role {
                    Role::Owner => "Owner",
                    Role::Mod => "Moderator",
                    Role::Member => "Online",
                }
            } else {
                "Offline"
            };
            ui.label(
                egui::RichText::new(status)
                    .size(10.5)
                    .color(palette.message_hint_text),
            );
        });
    }

    fn show_message_area(&mut self, ui: &mut egui::Ui, discord_dark: Option<DiscordDarkPalette>) {
        let palette = discord_dark.unwrap_or_else(|| {
            theme_discord_dark_palette(self.theme).unwrap_or_else(discord_dark_fallback_palette)
        });

        let pane_clip = ui.clip_rect();
        ui.set_clip_rect(pane_clip);

        egui::TopBottomPanel::top("channel_header")
            .resizable(false)
            .exact_height(48.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.message_background)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show_inside(ui, |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(pane_clip));

                if let Some(channel_id) = self.selected_channel {
                    if let Some(ch) = self.channels.iter().find(|c| c.channel_id == channel_id) {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new("#")
                                    .color(palette.nav_text)
                                    .size(20.0)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(&ch.name)
                                    .color(palette.title_text)
                                    .size(17.0)
                                    .strong(),
                            );
                            ui.separator();
                            ui.label(
                                egui::RichText::new("Channel conversation")
                                    .color(palette.nav_text)
                                    .size(13.0 * self.readability.text_scale),
                            );
                        });
                    }
                } else {
                    ui.label(
                        egui::RichText::new("Pick a channel to start chatting")
                            .color(palette.message_hint_text)
                            .size(13.0 * self.readability.text_scale),
                    );
                }
            });

        let composer_panel = egui::TopBottomPanel::bottom("composer")
            .resizable(true)
            .default_height(self.composer_panel_height.clamp(MIN_COMPOSER_PANEL_HEIGHT, 180.0))
            .min_height(MIN_COMPOSER_PANEL_HEIGHT)
            .max_height(180.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.message_background)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show_inside(ui, |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(pane_clip));

                egui::Frame::new()
                    .fill(palette.message_background) // exact match: no shade mismatch
                    .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                    .corner_radius(egui::CornerRadius::same(4))
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        let can_send = self.selected_channel.is_some();
                        let enabled = can_send && self.auth_session_established;

                        let panel_h = ui.available_height();
                        let row_height = (panel_h - 18.0).clamp(36.0, 72.0);
                        let send_width = (88.0 + (row_height - 36.0) * 0.28).clamp(88.0, 124.0);
                        let attachment_width = (38.0 + (row_height - 36.0) * 0.12).clamp(38.0, 48.0);

                        if !can_send {
                            ui.colored_label(palette.message_hint_text, "Pick a channel to enable the composer.");
                            ui.add_space(6.0);
                        }

                        ui.add_enabled_ui(enabled, |ui| {
                            ui.scope(|ui| {
                                let visuals = ui.visuals_mut();

                                visuals.widgets.inactive.bg_fill = palette.nav_item_active;
                                visuals.widgets.hovered.bg_fill = palette.nav_item_hover;
                                visuals.widgets.active.bg_fill = lighten_color(palette.nav_item_active, 0.08);

                                visuals.widgets.inactive.fg_stroke.color = palette.message_text;
                                visuals.widgets.hovered.fg_stroke.color = palette.title_text;
                                visuals.widgets.active.fg_stroke.color = palette.title_text;

                                visuals.widgets.inactive.bg_stroke.color = palette.nav_item_stroke;
                                visuals.widgets.hovered.bg_stroke.color = palette.nav_item_stroke_active;
                                visuals.widgets.active.bg_stroke.color = palette.nav_item_stroke_active;

                                ui.horizontal(|ui| {
                                    let attach_resp = ui
                                        .add_sized([attachment_width, row_height], egui::Button::new("📎"))
                                        .on_hover_text("Attach file");

                                    if attach_resp.clicked() {
                                        self.attachment_menu_anchor = Some(attach_resp.rect);
                                        self.attachment_menu_open = !self.attachment_menu_open;
                                    }

                                    let spacing = ui.spacing().item_spacing.x;
                                    let text_w = (ui.available_width() - send_width - spacing).max(64.0);

                                    let response = ui
                                        .scope(|ui| {
                                            let visuals = ui.visuals_mut();

                                            // Force TextEdit to exact pane color (no slightly lighter fill)
                                            visuals.extreme_bg_color = palette.message_background;
                                            visuals.widgets.inactive.bg_fill = palette.message_background;
                                            visuals.widgets.hovered.bg_fill = palette.message_background;
                                            visuals.widgets.active.bg_fill = palette.message_background;

                                            visuals.widgets.inactive.bg_stroke.color = palette.nav_item_stroke;
                                            visuals.widgets.hovered.bg_stroke.color = palette.nav_item_stroke_active;
                                            visuals.widgets.active.bg_stroke.color = palette.nav_item_stroke_active;

                                            visuals.override_text_color = Some(palette.message_text);

                                            // Make selection look sane in dark theme
                                            visuals.selection.bg_fill = palette.nav_item_active;
                                            visuals.selection.stroke = egui::Stroke::new(1.0, palette.nav_item_stroke_active);

                                            ui.add_sized(
                                                [text_w, row_height],
                                                egui::TextEdit::multiline(&mut self.composer)
                                                    .id_salt("composer_text")
                                                    .hint_text(
                                                        egui::RichText::new(
                                                            "Message #channel (Enter to send, Shift+Enter for newline)",
                                                        )
                                                        .color(palette.message_hint_text),
                                                    ),
                                            )
                                        })
                                        .inner;

                                    let send_shortcut = response.has_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);

                                    let clicked_send = ui
                                        .add_sized([send_width, row_height], egui::Button::new("⬆ Send"))
                                        .clicked();

                                    if send_shortcut || clicked_send {
                                        self.try_send_current_composer(&response);
                                    }
                                });

                                if let Some(path) = self.pending_attachment.clone() {
                                    ui.add_space(6.0);

                                    egui::ScrollArea::vertical()
                                        .max_height((panel_h - row_height - 18.0).max(0.0))
                                        .show(ui, |ui| {
                                            ui.set_clip_rect(ui.clip_rect().intersect(pane_clip));

                                            ui.horizontal_wrapped(|ui| {
                                                ui.colored_label(
                                                    palette.message_hint_text,
                                                    format!("Attached: {}", path.display()),
                                                );
                                                if ui.button("✕ Remove").clicked() {
                                                    self.pending_attachment = None;
                                                }
                                            });

                                            let file_name = path
                                                .file_name()
                                                .and_then(|name| name.to_str())
                                                .unwrap_or("attachment");
                                            let size_text = Self::attachment_size_text(&path);

                                            match self.load_attachment_preview(ui.ctx(), &path) {
                                                Some(AttachmentPreview::Image { texture, size, preview_png }) => {
                                                    egui::Frame::new()
                                                        .fill(palette.message_background)
                                                        .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                                                        .corner_radius(egui::CornerRadius::same(6))
                                                        .inner_margin(egui::Margin::symmetric(8, 6))
                                                        .show(ui, |ui| {
                                                            ui.horizontal(|ui| {
                                                                ui.colored_label(palette.message_text, format!("🖼 {file_name}"));
                                                                ui.colored_label(palette.message_hint_text, size_text.clone());
                                                            });

                                                            let img_resp = ui.add(
                                                                egui::Image::new((texture.id(), size))
                                                                    .max_size(egui::vec2(240.0, 240.0)),
                                                            );

                                                            let metadata = format!("name: {file_name}\nsize: {size_text}");
                                                            img_resp.context_menu(|ui| {
                                                                self.render_image_context_menu(
                                                                    ui,
                                                                    file_name,
                                                                    Some(preview_png.as_slice()),
                                                                    None,
                                                                    Some(&path),
                                                                    Some(metadata.as_str()),
                                                                );
                                                            });
                                                        });
                                                }
                                                Some(AttachmentPreview::DecodeFailed) => {
                                                    ui.colored_label(
                                                        ui.visuals().error_fg_color,
                                                        format!("Preview unavailable for {file_name} ({size_text})"),
                                                    );
                                                }
                                                None => {
                                                    ui.colored_label(
                                                        palette.message_hint_text,
                                                        format!("Attached: {file_name} ({size_text})"),
                                                    );
                                                }
                                            }
                                        });
                                }
                            });
                        });
                    });
            });

        self.composer_panel_height = composer_panel
            .response
            .rect
            .height()
            .clamp(MIN_COMPOSER_PANEL_HEIGHT, 180.0);

        if self.attachment_menu_open {
            if let Some(anchor) = self.attachment_menu_anchor {
                egui::Area::new(egui::Id::new("attach_menu_popup"))
                    .order(egui::Order::Foreground)
                    .pivot(egui::Align2::LEFT_BOTTOM)
                    .fixed_pos(egui::pos2(anchor.left(), anchor.top() - 8.0))
                    .show(ui.ctx(), |ui| {
                        ui.scope(|ui| {
                            let visuals = ui.visuals_mut();
                            visuals.widgets.inactive.bg_fill = palette.message_background;
                            visuals.widgets.hovered.bg_fill = palette.nav_item_hover;
                            visuals.widgets.active.bg_fill = palette.nav_item_active;
                            visuals.widgets.inactive.fg_stroke.color = palette.message_text;
                            visuals.widgets.hovered.fg_stroke.color = palette.title_text;
                            visuals.widgets.active.fg_stroke.color = palette.title_text;
                            visuals.widgets.inactive.bg_stroke.color = palette.nav_item_stroke;
                            visuals.widgets.hovered.bg_stroke.color =
                                palette.nav_item_stroke_active;
                            visuals.widgets.active.bg_stroke.color = palette.nav_item_stroke_active;
                            visuals.window_fill = palette.message_background;
                            visuals.panel_fill = palette.message_background;

                            egui::Frame::popup(ui.style())
                                .fill(palette.message_background)
                                .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                                .corner_radius(egui::CornerRadius::same(8))
                                .show(ui, |ui| {
                                    ui.set_min_width(220.0);
                                    ui.label(
                                        egui::RichText::new("Attach")
                                            .strong()
                                            .color(palette.title_text),
                                    );
                                    ui.separator();

                                    fn default_upload_dir() -> Option<std::path::PathBuf> {
                                        dirs::desktop_dir()
                                            .or_else(dirs::download_dir)
                                            .or_else(dirs::document_dir)
                                            .or_else(dirs::home_dir)
                                    }

                                    if ui.button("📄 File").clicked() {
                                        self.attachment_menu_open = false;
                                        let mut dialog = rfd::FileDialog::new();
                                        if let Some(dir) = default_upload_dir() {
                                            dialog = dialog.set_directory(dir);
                                        }
                                        self.pending_attachment = dialog.pick_file();
                                    }

                                    if ui.button("🖼 Image / GIF").clicked() {
                                        self.attachment_menu_open = false;
                                        let mut dialog = rfd::FileDialog::new().add_filter(
                                            "Images",
                                            &["png", "jpg", "jpeg", "gif", "webp", "bmp"],
                                        );
                                        if let Some(dir) = default_upload_dir() {
                                            dialog = dialog.set_directory(dir);
                                        }
                                        self.pending_attachment = dialog.pick_file();
                                    }

                                    if ui.button("✕ Close").clicked() {
                                        self.attachment_menu_open = false;
                                    }
                                });
                        });
                    });
            }
        }

        egui::ScrollArea::vertical()
            .id_salt(("messages_scroll", self.selected_channel))
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let scroll_clip = ui.clip_rect().intersect(pane_clip);
                ui.set_clip_rect(scroll_clip);

                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(4.0);

                if let Some(channel_id) = self.selected_channel {
                    if let Some(messages) = self.messages.get(&channel_id).cloned() {
                        for (i, msg) in messages.iter().enumerate() {
                            let starts_block =
                                i == 0 || messages[i - 1].wire.sender_id != msg.wire.sender_id;

                            self.render_mock_message_row(ui, msg, starts_block, palette);

                            if let Some(attachment) = msg.wire.attachment.as_ref() {
                                self.render_mock_message_attachment_row(
                                    ui,
                                    attachment,
                                    starts_block,
                                    palette,
                                );
                            }
                        }
                    } else {
                        ui.add_space(10.0);
                        ui.colored_label(
                            palette.message_hint_text,
                            "No messages yet. Send the first message.",
                        );
                    }
                } else {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Select a channel")
                                .size(18.0 * self.readability.text_scale)
                                .color(palette.title_text)
                                .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Choose a channel from the sidebar to view messages.",
                            )
                            .size(13.0 * self.readability.text_scale)
                            .color(palette.message_hint_text),
                        );
                    });
                }

                ui.add_space(8.0);
            });
    }

    fn render_mock_message_row(
        &mut self,
        ui: &mut egui::Ui,
        msg: &DisplayMessage,
        starts_block: bool,
        palette: DiscordDarkPalette,
    ) {
        const PAD_X: f32 = 10.0;
        const TEXT_GAP_X: f32 = 8.0;
        const MESSAGE_GAP: f32 = 6.0;
        const BLOCK_TOP: f32 = 5.0;
        const BLOCK_BOTTOM: f32 = 5.0;
        const CONT_TOP: f32 = MESSAGE_GAP * 0.5;
        const CONT_BOTTOM: f32 = MESSAGE_GAP * 0.5;

        let scale = self.readability.text_scale.clamp(0.85, 2.0);
        let compact = self.readability.compact_density;

        let pad_x = PAD_X * scale;
        let text_gap_x = TEXT_GAP_X * scale;

        let inset_top = if starts_block { BLOCK_TOP } else { CONT_TOP } * scale;
        let inset_bottom = if starts_block {
            BLOCK_BOTTOM
        } else {
            CONT_BOTTOM
        } * scale;

        let row_w = ui.available_width();

        let avatar_size = if compact { 28.0 } else { 36.0 } * scale;
        let avatar_slot_w = avatar_size + 8.0 * scale;

        let author_size = (if compact { 13.0 } else { 14.0 }) * scale;
        let time_size = 11.0 * scale;
        let body_size = (if compact { 12.5 } else { 13.5 }) * scale;

        let body_wrap_w = (row_w - pad_x * 2.0 - avatar_slot_w - text_gap_x).max(80.0);

        let body_galley = ui.fonts_mut(|fonts| {
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = body_wrap_w;
            job.append(
                &msg.plaintext,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(body_size),
                    color: palette.message_text,
                    ..Default::default()
                },
            );
            fonts.layout_job(job)
        });

        let measured_body_h = body_galley
            .size()
            .y
            .max((if compact { 14.0 } else { 16.0 }) * scale);

        let (author_galley, time_galley, header_line_h) = if starts_block {
            let author = msg
                .wire
                .sender_username
                .clone()
                .unwrap_or_else(|| msg.wire.sender_id.0.to_string());

            let local_dt = msg.wire.sent_at.with_timezone(&chrono::Local);
            let timestamp = if self.show_message_time_timezone {
                local_dt.format("%Y-%m-%d %H:%M %Z").to_string()
            } else {
                local_dt.format("%Y-%m-%d %H:%M").to_string()
            };

            let author_galley = ui.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    author,
                    egui::FontId::proportional(author_size),
                    palette.title_text,
                )
            });

            let time_galley = ui.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    timestamp,
                    egui::FontId::proportional(time_size),
                    palette.message_hint_text,
                )
            });

            let header_line_h = author_galley.size().y.max(time_galley.size().y);
            (Some(author_galley), Some(time_galley), header_line_h)
        } else {
            (None, None, 0.0)
        };

        let message_gap_px = MESSAGE_GAP * scale;

        let text_content_h = if starts_block {
            header_line_h + message_gap_px + measured_body_h
        } else {
            measured_body_h
        };

        let content_h = if starts_block {
            text_content_h.max(avatar_size)
        } else {
            text_content_h
        };

        let row_h = inset_top + content_h + inset_bottom;

        let (row_rect, _) = ui.allocate_exact_size(egui::vec2(row_w, row_h), egui::Sense::hover());
        let row_clip = row_rect.intersect(ui.clip_rect());

        let hovered = ui.rect_contains_pointer(row_rect);
        if hovered {
            ui.painter().with_clip_rect(row_clip).rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                palette.message_hover,
            );
        }

        let content = egui::Rect::from_min_max(
            egui::pos2(row_rect.left() + pad_x, row_rect.top() + inset_top),
            egui::pos2(row_rect.right() - pad_x, row_rect.bottom() - inset_bottom),
        );

        let x0 = content.left();
        let text_x = x0 + avatar_slot_w + text_gap_x;

        if starts_block {
            let avatar_y = content.top() + ((content_h - avatar_size) * 0.5).max(0.0);
            let avatar = egui::Rect::from_min_size(
                egui::pos2(x0, avatar_y),
                egui::vec2(avatar_size, avatar_size),
            );

            ui.painter().with_clip_rect(row_clip).rect_filled(
                avatar,
                egui::CornerRadius::same((avatar_size * 0.5).round() as u8),
                egui::Color32::from_rgb(89, 101, 125),
            );
        }

        let header_y = content.top();
        let body_y = if starts_block {
            header_y + header_line_h + message_gap_px
        } else {
            content.top()
        };

        // Optional bubble background for text body (readability toggle from old UI)
        if self.readability.message_bubble_backgrounds {
            let bubble_left = text_x - (6.0 * scale);
            let bubble_top = if starts_block {
                body_y - (3.0 * scale)
            } else {
                body_y - (1.0 * scale)
            };
            let bubble_rect = egui::Rect::from_min_max(
                egui::pos2(bubble_left, bubble_top),
                egui::pos2(
                    content.right(),
                    (body_y + measured_body_h + 4.0 * scale).min(row_rect.bottom()),
                ),
            );

            ui.painter().with_clip_rect(row_clip).rect_filled(
                bubble_rect,
                egui::CornerRadius::same(6),
                egui::Color32::from_rgba_unmultiplied(
                    palette.message_hover.r(),
                    palette.message_hover.g(),
                    palette.message_hover.b(),
                    if hovered { 34 } else { 20 },
                ),
            );
        }

        if starts_block {
            if let (Some(author_galley), Some(time_galley)) = (author_galley, time_galley) {
                let author_pos = egui::pos2(text_x, header_y);
                let time_pos = egui::pos2(
                    text_x + author_galley.size().x + 8.0 * scale,
                    header_y + (header_line_h - time_galley.size().y) * 0.5,
                );

                let painter = ui.painter().with_clip_rect(row_clip);
                painter.galley(author_pos, author_galley, palette.title_text);
                painter.galley(time_pos, time_galley, palette.message_hint_text);
            }
        }

        let body_rect = egui::Rect::from_min_size(
            egui::pos2(text_x, body_y),
            egui::vec2(
                (content.right() - text_x).max(20.0),
                measured_body_h.max(1.0),
            ),
        );

        ui_in_rect(ui, body_rect, |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(row_clip));
            ui.spacing_mut().item_spacing.y = 0.0;

            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(body_rect.width());
                ui.set_max_width(body_rect.width());

                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&msg.plaintext)
                            .color(palette.message_text)
                            .size(body_size),
                    )
                    .wrap()
                    .selectable(true),
                );
            });
        });
    }

    fn render_image_attachment_preview(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &AttachmentPayload,
        palette: DiscordDarkPalette,
    ) {
        let state = self
            .attachment_previews
            .entry(attachment.file_id)
            .or_insert(AttachmentPreviewState::NotRequested);

        if matches!(state, AttachmentPreviewState::NotRequested) {
            *state = AttachmentPreviewState::Loading;
            dispatch_backend_command(
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
                    ui.colored_label(
                        palette.message_hint_text,
                        format!("Loading preview for {}…", attachment.filename),
                    );
                });
            }
            AttachmentPreviewState::Error(reason) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Couldn't preview {}: {reason}", attachment.filename),
                );
                self.render_attachment_download_row(ui, attachment, palette);
            }
            AttachmentPreviewState::Ready {
                image,
                original_bytes,
                preview_png,
                texture,
                gif_animation,
            } => {
                let preview_image = image.clone();
                let full_bytes = original_bytes.clone();
                let preview_bytes = preview_png.clone();
                let mut chosen_texture: Option<egui::TextureHandle> = None;
                let mut preview_size_px = egui::vec2(image.width as f32, image.height as f32);

                if let Some(anim) = gif_animation.as_mut() {
                    let now = ui.ctx().input(|i| i.time);

                    if anim.texture.is_none() && !anim.frames.is_empty() {
                        let first = &anim.frames[0];
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [first.width, first.height],
                            &first.rgba,
                        );
                        anim.texture = Some(ui.ctx().load_texture(
                            format!("attachment_gif_preview_{}", attachment.file_id.0),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        ));
                        anim.current_frame = 0;
                        anim.next_frame_at_secs = now + (first.delay_ms as f64 / 1000.0);
                    }

                    while now >= anim.next_frame_at_secs && !anim.frames.is_empty() {
                        anim.current_frame = (anim.current_frame + 1) % anim.frames.len();
                        let frame = &anim.frames[anim.current_frame];

                        if let Some(tex) = anim.texture.as_mut() {
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [frame.width, frame.height],
                                &frame.rgba,
                            );
                            tex.set(color_image, egui::TextureOptions::LINEAR);
                        }

                        anim.next_frame_at_secs += frame.delay_ms as f64 / 1000.0;
                    }

                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(16));

                    if let Some(tex) = &anim.texture {
                        preview_size_px = tex.size_vec2();
                        chosen_texture = Some(tex.clone());
                    }
                } else {
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

                    if let Some(tex) = texture.as_ref() {
                        preview_size_px = tex.size_vec2();
                        chosen_texture = Some(tex.clone());
                    }
                }

                if let Some(texture) = chosen_texture {
                    let max_width = (ui.available_width() * 0.75).clamp(120.0, 380.0);
                    let mut preview_size = preview_size_px;
                    if preview_size.x > max_width {
                        preview_size *= max_width / preview_size.x;
                    }
                    if preview_size.y > 260.0 {
                        preview_size *= 260.0 / preview_size.y;
                    }

                    // IMPORTANT: do NOT use Frame::group(ui.style()) here, it uses egui defaults and
                    // causes the slight shade mismatch you’re seeing.
                    let response = egui::Frame::new()
                        .fill(palette.message_background)
                        .stroke(egui::Stroke::new(1.0, palette.nav_item_stroke))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(4, 4))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Button::image(
                                    egui::Image::new(&texture).fit_to_exact_size(preview_size),
                                )
                                .frame(false),
                            )
                        })
                        .inner
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

                    let is_gif = attachment.filename.to_ascii_lowercase().ends_with(".gif");
                    if is_gif {
                        ui.colored_label(
                            palette.message_hint_text,
                            format!(
                                "🖼 {} ({}) · GIF",
                                attachment.filename,
                                human_readable_bytes(attachment.size_bytes)
                            ),
                        );
                    } else {
                        ui.colored_label(
                            palette.message_hint_text,
                            format!(
                                "🖼 {} ({})",
                                attachment.filename,
                                human_readable_bytes(attachment.size_bytes)
                            ),
                        );
                    }
                }

                self.render_attachment_download_row(ui, attachment, palette);
            }
            AttachmentPreviewState::NotRequested => {}
        }
    }

    fn render_attachment_download_row(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &AttachmentPayload,
        palette: DiscordDarkPalette,
    ) {
        ui.add_space(2.0);

        ui.scope(|ui| {
            let visuals = ui.visuals_mut();

            visuals.widgets.inactive.bg_fill = palette.nav_item_active;
            visuals.widgets.hovered.bg_fill = palette.nav_item_hover;
            visuals.widgets.active.bg_fill = lighten_color(palette.nav_item_active, 0.08);

            visuals.widgets.inactive.bg_stroke.color = palette.nav_item_stroke;
            visuals.widgets.hovered.bg_stroke.color = palette.nav_item_stroke_active;
            visuals.widgets.active.bg_stroke.color = palette.nav_item_stroke_active;

            visuals.widgets.inactive.fg_stroke.color = palette.message_text;
            visuals.widgets.hovered.fg_stroke.color = palette.title_text;
            visuals.widgets.active.fg_stroke.color = palette.title_text;

            // Reduce vertical padding so the button is shorter (less likely to clip),
            // and keep width compact.
            ui.spacing_mut().button_padding.y = 1.0;

            ui.horizontal(|ui| {
                let h = (20.0 * self.readability.text_scale.clamp(0.9, 1.2)).round();
                let w = (84.0 * self.readability.text_scale.clamp(0.9, 1.15)).round();

                if ui
                    .add_sized([w, h], egui::Button::new("Download"))
                    .clicked()
                {
                    dispatch_backend_command(
                        &self.cmd_tx,
                        BackendCommand::DownloadAttachment {
                            file_id: attachment.file_id,
                            filename: attachment.filename.clone(),
                        },
                        &mut self.status,
                    );
                }
            });
        });

        // Extra bottom room so hover/background extends past the text baseline.
        ui.add_space(4.0);
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
                    texture,
                    gif_animation,
                    ..
                }) = self.attachment_previews.get(&file_id)
                {
                    let chosen_texture = gif_animation
                        .as_ref()
                        .and_then(|g| g.texture.as_ref())
                        .or(texture.as_ref());

                    if let Some(texture) = chosen_texture {
                        let max_size = ui.available_size();
                        let mut size = texture.size_vec2();
                        let scale = (max_size.x / size.x).min(max_size.y / size.y).min(1.0);
                        size *= scale;
                        ui.add(egui::Image::new(texture).fit_to_exact_size(size));
                    } else {
                        ui.label("Preview not available.");
                    }
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
fn decode_first_gif_frame_image(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    use image::AnimationDecoder;
    use std::io::Cursor;

    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map_err(|e| format!("gif decode init failed: {e}"))?;

    let mut frames = decoder.into_frames();
    let first = frames
        .next()
        .transpose()
        .map_err(|e| format!("gif frame decode failed: {e}"))?
        .ok_or_else(|| "gif has no frames".to_string())?;

    Ok(image::DynamicImage::ImageRgba8(first.into_buffer()))
}

fn decode_gif_animation_preview(bytes: &[u8]) -> Result<Option<DecodedGifPreview>, String> {
    use image::AnimationDecoder;
    use std::io::Cursor;

    // Only try GIF decode for GIF files
    let is_gif = bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a");
    if !is_gif {
        return Ok(None);
    }

    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map_err(|e| format!("gif decode init failed: {e}"))?;

    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| format!("gif frame decode failed: {e}"))?;

    if frames.is_empty() {
        return Ok(None);
    }

    let mut out_frames = Vec::with_capacity(frames.len());

    for frame in frames {
        let delay = frame.delay();
        let (num, den) = delay.numer_denom_ms();
        let delay_ms = if den == 0 {
            100
        } else {
            // round, clamp to avoid absurdly-fast frame spam
            ((num as f32 / den as f32).round() as u32).clamp(20, 10_000)
        };

        let rgba = frame.into_buffer();
        let resized = image::DynamicImage::ImageRgba8(rgba)
            .thumbnail(1024, 1024)
            .to_rgba8();

        out_frames.push(AnimatedGifFrame {
            width: resized.width() as usize,
            height: resized.height() as usize,
            rgba: resized.into_raw(),
            delay_ms,
        });
    }

    Ok(Some(DecodedGifPreview { frames: out_frames }))
}
#[derive(Debug, Clone, Copy)]
struct DiscordDarkPalette {
    // Backgrounds:
    app_background: egui::Color32,
    nav_background: egui::Color32,
    message_background: egui::Color32,
    message_hover: egui::Color32,
    members_background: egui::Color32,
    members_hover: egui::Color32,

    // Main Text:
    nav_text: egui::Color32,
    nav_text_hover: egui::Color32,
    nav_text_highlighted: egui::Color32,
    message_text: egui::Color32,
    message_hint_text: egui::Color32,

    // Title Text:
    title_text: egui::Color32,
    nav_title_text: egui::Color32,

    // Navigator Item Styling:
    nav_item_hover: egui::Color32,
    nav_item_active: egui::Color32,
    nav_item_stroke: egui::Color32,
    nav_item_stroke_active: egui::Color32,
}
fn theme_discord_dark_palette(theme: ThemeSettings) -> Option<DiscordDarkPalette> {
    (theme.preset == ThemePreset::DiscordDark).then_some({
        DiscordDarkPalette {
            // Backgrounds:
            app_background: egui::Color32::from_rgb(26, 26, 30),
            nav_background: egui::Color32::from_rgb(18, 18, 20),
            message_background: egui::Color32::from_rgb(26, 26, 30),
            message_hover: egui::Color32::from_rgb(36, 36, 40),
            members_background: egui::Color32::from_rgb(26, 26, 30),
            members_hover: egui::Color32::from_rgb(43, 45, 49),
            // Main Text:
            nav_text: egui::Color32::from_rgb(129, 130, 138),
            nav_text_hover: egui::Color32::from_rgb(251, 251, 251),
            nav_text_highlighted: egui::Color32::from_rgb(251, 251, 251),
            message_text: egui::Color32::from_rgb(239, 239, 241),
            message_hint_text: egui::Color32::from_rgb(108, 109, 118),
            // Title Text:
            title_text: egui::Color32::from_rgb(251, 251, 251),
            nav_title_text: egui::Color32::from_rgb(239, 239, 241),
            // Navigator Item Styling:
            nav_item_hover: egui::Color32::from_rgb(29, 29, 30),
            nav_item_active: egui::Color32::from_rgb(44, 44, 48),
            nav_item_stroke: egui::Color32::from_rgb(48, 48, 56),
            nav_item_stroke_active: egui::Color32::from_rgb(92, 92, 105),
        }
    })
}

fn discord_dark_fallback_palette() -> DiscordDarkPalette {
    theme_discord_dark_palette(ThemeSettings::discord_default())
        .expect("DiscordDark fallback palette should always exist")
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

fn visuals_for_theme(theme: ThemeSettings) -> egui::Visuals {
    let mut visuals = match theme.preset {
        ThemePreset::DiscordDark => {
            let mut v = egui::Visuals::dark();
            let palette = theme_discord_dark_palette(theme)
                .expect("DiscordDark palette should exist for DiscordDark preset");
            v.override_text_color = None;
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
    visuals.window_corner_radius = egui::CornerRadius::same(theme.panel_rounding);
    visuals.menu_corner_radius = egui::CornerRadius::same(theme.panel_rounding);
    visuals.selection.bg_fill = theme.accent_color;
    visuals.widgets.active.bg_fill = theme.accent_color;
    visuals.widgets.hovered.bg_fill = theme.accent_color.gamma_multiply(0.85);

    // Popup/menu polish so menu_button dropdowns match the active theme.
    let popup_radius = theme.panel_rounding.clamp(4, 16);
    visuals.menu_corner_radius = egui::CornerRadius::same(popup_radius);
    visuals.window_corner_radius = egui::CornerRadius::same(popup_radius.saturating_add(2));

    if let Some(palette) = theme_discord_dark_palette(theme) {
        visuals.window_fill = palette.nav_background;
        visuals.panel_fill = palette.app_background;
        visuals.window_stroke = egui::Stroke::new(1.0, palette.nav_item_stroke);
        visuals.widgets.noninteractive.bg_fill = palette.nav_background;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette.nav_item_stroke);
        visuals.widgets.inactive.bg_fill = palette.nav_item_hover;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette.nav_item_stroke);
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.nav_item_stroke_active);
    }

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
const DESKTOP_GUI_DEVICE_ID_PREFIX: &str = "desktop-gui";

fn sanitize_profile_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.');
        out.push(if ok { ch } else { '_' });
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "user".to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

fn desktop_gui_device_id_for_username(username: &str) -> String {
    format!(
        "{DESKTOP_GUI_DEVICE_ID_PREFIX}-{}",
        sanitize_profile_segment(username)
    )
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    user_id: i64,
}

fn icon_btn(
    icon: &str,
    color: egui::Color32,
    active_bg: Option<egui::Color32>,
) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(icon).color(color))
        .min_size(egui::vec2(24.0, 24.0))
        .stroke(egui::Stroke::NONE)
        .fill(active_bg.unwrap_or(egui::Color32::TRANSPARENT))
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

    // 1) Explicit override wins (handy for scripts / testing)
    if let Some(dir) = read_non_empty_env_var("PROTO_RTC_CLIENTS_DIR", &mut attempts) {
        return Ok(PathBuf::from(dir));
    }

    // 2) Default to repo-local ./data/clients when launched from repo
    if let Ok(cwd) = std::env::current_dir() {
        return Ok(cwd.join("data").join("clients"));
    }

    // 3) Fallback to OS user profile location
    if let Some(home) = read_non_empty_env_var("HOME", &mut attempts) {
        return Ok(PathBuf::from(home).join(".proto_rtc").join("clients"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(userprofile) = read_non_empty_env_var("USERPROFILE", &mut attempts) {
            return Ok(PathBuf::from(userprofile)
                .join(".proto_rtc")
                .join("clients"));
        }

        let homedrive = read_non_empty_env_var("HOMEDRIVE", &mut attempts);
        let homepath = read_non_empty_env_var("HOMEPATH", &mut attempts);
        match (homedrive, homepath) {
            (Some(homedrive), Some(homepath)) => {
                return Ok(PathBuf::from(format!("{homedrive}{homepath}"))
                    .join(".proto_rtc")
                    .join("clients"));
            }
            _ => {
                attempts.push(
                    "HOMEDRIVE+HOMEPATH could not be used because one or both were unavailable"
                        .to_string(),
                );
            }
        }

        if let Some(local_app_data) = read_non_empty_env_var("LOCALAPPDATA", &mut attempts) {
            return Ok(PathBuf::from(local_app_data)
                .join("proto_rtc")
                .join("clients"));
        }
    }

    Err(format!(
        "could not resolve a client state root (checked PROTO_RTC_CLIENTS_DIR, current_dir, HOME{}): {}",
        if cfg!(target_os = "windows") {
            ", USERPROFILE, HOMEDRIVE+HOMEPATH, LOCALAPPDATA"
        } else {
            ""
        },
        attempts.join("; ")
    ))
}

fn resolve_user_profile_data_dir(base_dir: &std::path::Path, username: &str) -> PathBuf {
    base_dir.join(sanitize_profile_segment(username))
}

fn resolve_user_mls_data_dir(base_dir: &std::path::Path, username: &str, user_id: i64) -> PathBuf {
    resolve_user_profile_data_dir(base_dir, username)
        .join("mls")
        .join(format!("user_{user_id}"))
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

fn guild_id_from_invite(invite_code: &str) -> Option<GuildId> {
    let decoded = URL_SAFE_NO_PAD.decode(invite_code.as_bytes()).ok()?;
    let decoded_text = String::from_utf8(decoded).ok()?;
    let guild_id = decoded_text.strip_prefix("guild:")?.parse::<i64>().ok()?;
    Some(GuildId(guild_id))
}

async fn build_user_scoped_mls_client(
    base_dir: &std::path::Path,
    username: &str,
    user_id: i64,
) -> Result<Arc<RealtimeClient<PassthroughCrypto>>, String> {
    let profile_dir = resolve_user_profile_data_dir(base_dir, username);
    let user_mls_state_dir = resolve_user_mls_data_dir(base_dir, username, user_id);

    // Create profile directories up-front so all local state is clearly scoped
    std::fs::create_dir_all(profile_dir.join("downloads")).map_err(|err| {
        format!(
            "could not prepare profile directory '{}' for username='{}': {err}",
            profile_dir.display(),
            username
        )
    })?;
    std::fs::create_dir_all(profile_dir.join("cache")).map_err(|err| {
        format!(
            "could not prepare cache directory '{}' for username='{}': {err}",
            profile_dir.join("cache").display(),
            username
        )
    })?;
    std::fs::create_dir_all(&user_mls_state_dir).map_err(|err| {
        format!(
            "could not prepare per-user MLS state directory '{}' for username='{}' user_id={user_id}: {err}",
            user_mls_state_dir.display(),
            username
        )
    })?;

    let mls_db_url = DurableMlsSessionManager::sqlite_url_for_gui_data_dir(&user_mls_state_dir);
    let device_id = desktop_gui_device_id_for_username(username);

    let mls_manager = DurableMlsSessionManager::initialize(&mls_db_url, user_id, &device_id)
        .await
        .map_err(|err| {
            format!(
                "failed to initialize persistent MLS backend for username='{}' user_id={} device_id='{}' ({}): {:#}",
                username, user_id, device_id, mls_db_url, err
            )
        })?;

    Ok(RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        mls_manager,
    ))
}

pub fn start_backend_bridge(cmd_rx: Receiver<BackendCommand>, ui_tx: Sender<UiEvent>) {
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

                        let rebound_client = match build_user_scoped_mls_client(&mls_state_dir, &username, user_id).await {
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
                        tracing::info!("backend: list_guilds");
                        if let Err(err) = client.list_guilds().await {
                            tracing::error!("backend: list_guilds failed: {err}");
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::ListChannels { guild_id } => {
                        tracing::info!(guild_id = guild_id.0, "backend: list_channels");
                        if let Err(err) = client.list_channels(guild_id).await {
                            tracing::error!(guild_id = guild_id.0, "backend: list_channels failed: {err}");
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::ListMembers { guild_id } => {
                        tracing::info!(guild_id = guild_id.0, "backend: list_members");
                        if let Err(err) = client.list_members(guild_id).await {
                            tracing::error!(guild_id = guild_id.0, "backend: list_members failed: {err}");
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::General,
                                err.to_string(),
                            )));
                        }
                    }
                    BackendCommand::SelectChannel { channel_id } => {
                        tracing::info!(channel_id = channel_id.0, "backend: select_channel");
                        if let Err(err) = client.select_channel(channel_id).await {
                            tracing::error!(channel_id = channel_id.0, "backend: select_channel failed: {err}");
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
                        tracing::info!(
                            has_attachment = attachment_path.is_some(),
                            text_len = text.len(),
                            "backend: send_message"
                        );
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
                            tracing::error!("backend: send_message failed: {err}");
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

                                let decoded_gif = match decode_gif_animation_preview(&owned) {
                                    Ok(v) => v,
                                    Err(err) => {
                                        tracing::warn!(
                                            "gif preview decode failed for {}: {}",
                                            file_id.0,
                                            err
                                        );
                                        None
                                    }
                                };

                                match decode_preview_image(&owned) {
                                    Ok(image) => {
                                        let _ = ui_tx.try_send(UiEvent::AttachmentPreviewLoaded {
                                            file_id,
                                            image,
                                            original_bytes: owned,
                                            decoded_gif,
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
                        tracing::info!("backend: join_with_invite");
                        match client.join_with_invite(&invite_code).await {
                            Ok(()) => {
                                tracing::info!("backend: join_with_invite succeeded");
                                if let Some(guild_id) = guild_id_from_invite(&invite_code) {
                                    let _ = ui_tx.try_send(UiEvent::JoinedGuild(guild_id));
                                }
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
                                tracing::error!("backend: join_with_invite failed: {err}");
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
