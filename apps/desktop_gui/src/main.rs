use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    thread,
    time::SystemTime,
};

use arboard::{Clipboard, ImageData};
use client_core::{
    AttachmentUpload, ClientEvent, ClientHandle, DurableMlsSessionManager, PassthroughCrypto,
    RealtimeClient, VoiceConnectOptions, VoiceParticipantState, VoiceSessionSnapshot,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use eframe::egui;
use egui::TextureHandle;
use image::GenericImageView;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceSessionConnectionStatus {
    Idle,
    Connecting,
    Connected,
    Disconnecting,
    Error,
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
}

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
        }
    }
}

impl PersistedDesktopSettings {
    fn into_runtime(self) -> (ThemeSettings, UiReadabilitySettings) {
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
        )
    }

    fn from_runtime(theme: ThemeSettings, readability: UiReadabilitySettings) -> Self {
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
    Invite,
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
    password_or_invite: String,
    auth_session_established: bool,

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

    voice_ui: VoiceSessionUiState,

    settings_open: bool,
    view_state: AppViewState,

    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
    readability: UiReadabilitySettings,
    applied_readability: Option<UiReadabilitySettings>,

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
        let (theme, readability) = persisted_settings.unwrap_or_default().into_runtime();
        Self {
            cmd_tx,
            ui_rx,
            server_url: "http://127.0.0.1:8443".to_string(),
            username: "alice".to_string(),
            password_or_invite: String::new(),
            auth_session_established: false,
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
            voice_ui: VoiceSessionUiState::new(),
            settings_open: false,
            view_state: AppViewState::Login,
            theme,
            applied_theme: None,
            readability,
            applied_readability: None,
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
                    self.password_or_invite = invite_code;
                    self.status =
                        "Invite code created; share this code with another user".to_string();
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
                                | UiErrorContext::DecryptMessage
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

                        // We defer Join Invite actions until after UI closures end (prevents &mut self reborrow)
                        let mut join_invite_request: Option<String> = None;

                        // Fields (stacked)
                        egui::Frame::none()
                            .fill(ui.visuals().faint_bg_color.gamma_multiply(0.55))
                            .rounding(12.0)
                            .inner_margin(egui::Margin::symmetric(14.0, 12.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Account").strong());
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

                        // Invite section (stacked + clearer)
                        egui::Frame::none()
                            .fill(ui.visuals().faint_bg_color.gamma_multiply(0.45))
                            .rounding(12.0)
                            .inner_margin(egui::Margin::symmetric(14.0, 12.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Invite").strong());
                                ui.weak("Paste an invite code to join a guild (after signing in).");
                                ui.add_space(6.0);
                                let mut invite_buf = self.password_or_invite.clone();
                                // Invite field
                                let invite_resp = self.login_text_field(
                                    ui,
                                    "login_invite",
                                    "Invite code",
                                    "XXXX-XXXX",
                                    &mut invite_buf,
                                    focus_to_set == Some(LoginFocusField::Invite),
                                );

                                ui.add_space(6.0);

                                let can_join = self.auth_session_established
                                    && !self.password_or_invite.trim().is_empty();

                                ui.horizontal(|ui| {
                                    if ui.button("Paste").clicked() {
                                        if let Ok(mut clipboard) = Clipboard::new() {
                                            if let Ok(text) = clipboard.get_text() {
                                                invite_buf = text;
                                            }
                                        }
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    can_join,
                                                    egui::Button::new("Join Invite")
                                                        .min_size(egui::vec2(140.0, 32.0)),
                                                )
                                                .clicked()
                                            {
                                                join_invite_request = Some(
                                                    self.password_or_invite.trim().to_string(),
                                                );
                                            }
                                        },
                                    );
                                });
                                self.password_or_invite = invite_buf;

                                // Enter joins if invite field has focus
                                let enter_pressed = ctx.input(|i| i.key_pressed(egui::Key::Enter));
                                if invite_resp.has_focus() && enter_pressed && can_join {
                                    join_invite_request =
                                        Some(self.password_or_invite.trim().to_string());
                                }
                            });

                        // Execute deferred join request AFTER UI closures (fixes borrow checker error)
                        if let Some(invite_code) = join_invite_request {
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::JoinWithInvite { invite_code },
                                &mut self.status,
                            );
                        }

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

    // ------------------- Main workspace (mostly unchanged) -------------------

    fn show_main_workspace(&mut self, ctx: &egui::Context) {
        const TOOLBAR_H_PADDING: f32 = 12.0;
        const TOOLBAR_V_PADDING: f32 = 8.0;
        const SECTION_VERTICAL_GAP: f32 = 8.0;
        const STATUS_VERTICAL_MARGIN: f32 = 6.0;

        let top_bar_bg = theme_discord_dark_palette(self.theme)
            .map(|p| p.navigation_background)
            .unwrap_or(ctx.style().visuals.panel_fill);

        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::none()
                    .fill(top_bar_bg)
                    .inner_margin(egui::Margin::symmetric(
                        TOOLBAR_H_PADDING,
                        TOOLBAR_V_PADDING,
                    )),
            )
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.settings_open = true;
                    }

                    ui.menu_button("Account", |ui| {
                        ui.label("Signed in as current user");
                        if ui.button("Sign out").clicked() {
                            self.auth_session_established = false;
                            self.view_state = AppViewState::Login;
                            self.status = "Signed out".to_string();
                            self.status_banner = None;
                            ui.close_menu();
                        }
                    });
                });

                ui.add_space(SECTION_VERTICAL_GAP);

                ui.horizontal_wrapped(|ui| {
                    if ui.button("Refresh Guilds").clicked() {
                        queue_command(&self.cmd_tx, BackendCommand::ListGuilds, &mut self.status);
                    }

                    if ui.button("Join Invite").clicked() {
                        let invite_code = self.password_or_invite.trim().to_string();
                        if invite_code.is_empty() {
                            self.status = "Enter an invite code first".to_string();
                        } else {
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::JoinWithInvite { invite_code },
                                &mut self.status,
                            );
                        }
                    }
                });

                ui.add_space(STATUS_VERTICAL_MARGIN);
                ui.label(&self.status);
                self.show_status_banner(ui);
                ui.add_space(STATUS_VERTICAL_MARGIN);
            });

        self.show_settings_window(ctx);

        let nav_bg = theme_discord_dark_palette(self.theme)
            .map(|p| p.navigation_background)
            .unwrap_or(ctx.style().visuals.panel_fill);

        egui::SidePanel::left("guilds_panel")
            .default_width(160.0)
            .frame(
                egui::Frame::none()
                    .fill(nav_bg)
                    .inner_margin(egui::Margin::symmetric(
                        TOOLBAR_H_PADDING,
                        TOOLBAR_V_PADDING,
                    )),
            )
            .show(ctx, |ui| {
                ui.heading("Guilds");
                let discord_dark = theme_discord_dark_palette(self.theme);
                ui.add_space(SECTION_VERTICAL_GAP);

                ui.add_enabled_ui(self.auth_session_established, |ui| {
                    if let Some(guild_id) = self.selected_guild {
                        let label = if let Some(palette) = discord_dark {
                            egui::RichText::new("Create Invite")
                                .color(palette.side_panel_button_text)
                        } else {
                            egui::RichText::new("Create Invite")
                        };
                        let mut invite_button = egui::Button::new(label)
                            .min_size(egui::vec2(ui.available_width(), 30.0));
                        if let Some(palette) = discord_dark {
                            invite_button = invite_button
                                .fill(palette.side_panel_button_fill)
                                .stroke(egui::Stroke::new(1.0, palette.guild_entry_stroke));
                        }
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
                        let selected = self.selected_guild == Some(guild.guild_id);
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
                                ui.visuals().selection.bg_fill.gamma_multiply(
                                    if self.theme.list_row_shading {
                                        0.35
                                    } else {
                                        0.22
                                    },
                                )
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
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().selection.bg_fill.gamma_multiply(0.9),
                                    )
                                } else if self.theme.list_row_shading {
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    )
                                } else {
                                    egui::Stroke::NONE
                                }
                            });

                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 34.0),
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
                            guild.name.as_str(),
                            egui::TextStyle::Button.resolve(ui.style()),
                            text_color,
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

                if !self.auth_session_established {
                    ui.separator();
                    ui.add_space(4.0);
                    ui.label("Sign in to access guilds.");
                }
            });

        egui::SidePanel::left("channels_panel")
            .default_width(260.0)
            .frame(
                egui::Frame::none()
                    .fill(nav_bg)
                    .inner_margin(egui::Margin::symmetric(
                        TOOLBAR_H_PADDING,
                        TOOLBAR_V_PADDING,
                    )),
            )
            .show(ctx, |ui| {
                let discord_dark = theme_discord_dark_palette(self.theme);
                ui.horizontal(|ui| {
                    ui.heading("Channels");
                    let refresh_label = if let Some(palette) = discord_dark {
                        egui::RichText::new("Refresh").color(palette.side_panel_button_text)
                    } else {
                        egui::RichText::new("Refresh")
                    };
                    let mut refresh_button = egui::Button::new(refresh_label);
                    if let Some(palette) = discord_dark {
                        refresh_button = refresh_button
                            .fill(palette.side_panel_button_fill)
                            .stroke(egui::Stroke::new(1.0, palette.guild_entry_stroke));
                    }
                    if ui.add(refresh_button).clicked() {
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
                });
                ui.add_space(SECTION_VERTICAL_GAP);

                ui.add_enabled_ui(self.auth_session_established, |ui| {
                    for channel in &self.channels {
                        let icon = match channel.kind {
                            ChannelKind::Text => "#",
                            ChannelKind::Voice => "🔊",
                        };
                        let selected = self.selected_channel == Some(channel.channel_id);
                        let discord_dark = theme_discord_dark_palette(self.theme);

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
                                ui.visuals().selection.bg_fill.gamma_multiply(
                                    if self.theme.list_row_shading {
                                        0.35
                                    } else {
                                        0.22
                                    },
                                )
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
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().selection.bg_fill.gamma_multiply(0.9),
                                    )
                                } else if self.theme.list_row_shading {
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    )
                                } else {
                                    egui::Stroke::NONE
                                }
                            });

                        let occupancy = if channel.kind == ChannelKind::Voice {
                            Some(self.voice_ui.occupancy_for_channel(channel.channel_id))
                        } else {
                            None
                        };
                        let connection_hint =
                            if self.voice_ui.is_channel_connected(channel.channel_id) {
                                Some("connected")
                            } else {
                                None
                            };

                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 36.0),
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

                        let channel_label = match (occupancy, connection_hint) {
                            (Some(count), Some(hint)) => {
                                format!("{icon}  {} ({count}) {hint}", channel.name)
                            }
                            (Some(count), None) => format!("{icon}  {} ({count})", channel.name),
                            (None, _) => format!("{icon}  {}", channel.name),
                        };

                        ui.painter().text(
                            rect.left_center() + egui::vec2(10.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            channel_label,
                            egui::TextStyle::Button.resolve(ui.style()),
                            text_color,
                        );

                        if response.clicked() {
                            self.selected_channel = Some(channel.channel_id);
                            if channel.kind == ChannelKind::Voice {
                                self.voice_ui.selected_voice_channel = Some(channel.channel_id);
                            }
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::SelectChannel {
                                    channel_id: channel.channel_id,
                                },
                                &mut self.status,
                            );
                        }

                        ui.add_space(6.0);
                    }

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
                        let mut cta_button = egui::Button::new(cta_text)
                            .min_size(egui::vec2(ui.available_width(), 30.0));
                        if let Some(palette) = discord_dark {
                            cta_button = cta_button
                                .fill(palette.side_panel_button_fill)
                                .stroke(egui::Stroke::new(1.0, palette.guild_entry_stroke));
                        }
                        if ui.add(cta_button).clicked() {
                            if connected_here {
                                self.voice_ui.connection_status =
                                    VoiceSessionConnectionStatus::Disconnecting;
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::DisconnectVoice,
                                    &mut self.status,
                                );
                            } else {
                                self.voice_ui.connection_status =
                                    VoiceSessionConnectionStatus::Connecting;
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
                });

                if !self.auth_session_established {
                    ui.separator();
                    ui.add_space(4.0);
                    ui.label("Sign in to browse channels.");
                }
            });

        egui::SidePanel::right("members_panel")
            .default_width(260.0)
            .show(ctx, |ui| {
                let members_bg = theme_discord_dark_palette(self.theme)
                    .map(|p| p.members_background)
                    .unwrap_or(ui.visuals().panel_fill);
                egui::Frame::none()
                    .fill(members_bg)
                    .inner_margin(egui::Margin::symmetric(
                        TOOLBAR_H_PADDING,
                        TOOLBAR_V_PADDING,
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
                        ui.add_space(SECTION_VERTICAL_GAP);

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
                        ui.add_space(SECTION_VERTICAL_GAP);
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
                                ui.toggle_value(
                                    &mut self.voice_ui.screen_share_enabled,
                                    "Screen share",
                                );
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

        egui::TopBottomPanel::bottom("composer_panel")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                let can_send = self.selected_channel.is_some();
                ui.add_enabled_ui(can_send && self.auth_session_established, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("📎").on_hover_text("Attach file").clicked() {
                            self.pending_attachment = rfd::FileDialog::new().pick_file();
                        }
                        let response = ui
                            .scope(|ui| {
                                if let Some(palette) = theme_discord_dark_palette(self.theme) {
                                    ui.visuals_mut().extreme_bg_color = palette.message_background;
                                }
                                ui.add_sized(
                                    [ui.available_width() - 140.0, 74.0],
                                    egui::TextEdit::multiline(&mut self.composer)
                                        .id_source("composer_text")
                                        .hint_text(
                                            "Message #channel (Enter to send, Shift+Enter for newline)",
                                        ),
                                )
                            })
                            .inner;
                        let send_shortcut = response.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        let clicked_send = ui
                            .add_sized([92.0, 74.0], egui::Button::new("⬆ Send"))
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
                                    let metadata = format!("name: {file_name}\nsize: {size_text}");
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
                if !can_send {
                    ui.centered_and_justified(|ui| {
                        ui.weak("Pick a channel to start chatting.");
                    });
                }
                ui.add_space(4.0);
            });

        let message_panel_bg = theme_discord_dark_palette(self.theme)
            .map(|p| p.message_background)
            .unwrap_or(ctx.style().visuals.panel_fill);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(message_panel_bg).inner_margin(
                egui::Margin::symmetric(TOOLBAR_H_PADDING, TOOLBAR_V_PADDING),
            ))
            .show(ctx, |ui| {
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
                ui.add_space(SECTION_VERTICAL_GAP);
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
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
                                let frame = if self.readability.message_bubble_backgrounds {
                                    egui::Frame::none().fill(base_message_bg)
                                } else {
                                    egui::Frame::none()
                                };
                                let message_response = frame
                                    .rounding(egui::Rounding::same(f32::from(
                                        self.theme.panel_rounding,
                                    )))
                                    .inner_margin(message_margin)
                                    .show(ui, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(egui::RichText::new(sender_display).strong());
                                            if self.readability.show_timestamps {
                                                ui.label(
                                                    egui::RichText::new(sent_at).small().weak(),
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
                                                        human_readable_bytes(attachment.size_bytes)
                                                    ));
                                                    if ui.button("Download").clicked() {
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
                                    if let Some(palette) = discord_dark {
                                        ui.ctx()
                                            .layer_painter(egui::LayerId::new(
                                                egui::Order::Background,
                                                egui::Id::new("message_hover_row"),
                                            ))
                                            .rect_filled(
                                                message_response.rect,
                                                egui::Rounding::same(f32::from(
                                                    self.theme.panel_rounding,
                                                )),
                                                palette.message_row_hover,
                                            );
                                    }
                                }

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
    message_row_hover: egui::Color32,
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

fn theme_discord_dark_palette(theme: ThemeSettings) -> Option<DiscordDarkPalette> {
    (theme.preset == ThemePreset::DiscordDark).then_some(DiscordDarkPalette {
        app_background: egui::Color32::from_rgb(26, 26, 30),
        message_background: egui::Color32::from_rgb(26, 26, 30),
        members_background: egui::Color32::from_rgb(26, 26, 30),
        navigation_background: egui::Color32::from_rgb(18, 18, 20),
        message_row_hover: egui::Color32::from_rgb(36, 36, 40),
        guild_entry_hover: egui::Color32::from_rgb(29, 29, 30),
        guild_entry_active: egui::Color32::from_rgb(44, 44, 48),
        guild_entry_stroke: egui::Color32::from_rgb(48, 48, 56),
        guild_entry_stroke_active: egui::Color32::from_rgb(92, 92, 105),
        guild_text_unhighlighted: egui::Color32::from_rgb(129, 130, 138),
        guild_text_hovered: egui::Color32::from_rgb(214, 216, 220),
        guild_text_highlighted: egui::Color32::from_rgb(251, 251, 251),
        side_panel_button_fill: egui::Color32::from_rgb(35, 35, 40),
        side_panel_button_text: egui::Color32::from_rgb(236, 237, 240),
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

impl eframe::App for DesktopGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick = self.tick.wrapping_add(1);

        self.process_ui_events();
        self.apply_theme_if_needed(ctx);

        match self.view_state {
            AppViewState::Login => self.show_login_screen(ctx),
            AppViewState::Main => self.show_main_workspace(ctx),
        }

        ctx.request_repaint();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = PersistedDesktopSettings::from_runtime(self.theme, self.readability);
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

            let mls_db_url = DurableMlsSessionManager::sqlite_url_for_gui_data_dir(&mls_state_dir);

            let mls_manager =
                match DurableMlsSessionManager::initialize(&mls_db_url, 0, "desktop-gui").await {
                    Ok(manager) => manager,
                    Err(err) => {
                        let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                            UiErrorContext::BackendStartup,
                            format!(
                                "backend worker startup failure: failed to initialize persistent MLS backend ({mls_db_url}): {err:#}"
                            ),
                        )));
                        tracing::error!(
                            "failed to initialize persistent MLS backend ({mls_db_url}): {err:#}"
                        );
                        return;
                    }
                };

            let client =
                RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, mls_manager);
            let _ = ui_tx.try_send(UiEvent::Info("Backend worker ready".to_string()));

            let mut subscribed = false;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BackendCommand::Login {
                        server_url,
                        username,
                    } => match client.login(&server_url, &username, "").await {
                        Ok(()) => {
                            if !subscribed {
                                subscribed = true;
                                let mut events = client.subscribe_events();
                                let ui_tx_clone = ui_tx.clone();
                                tokio::spawn(async move {
                                    while let Ok(event) = events.recv().await {
                                        let evt = match event {
                                            ClientEvent::Server(evt) => UiEvent::Server(evt),
                                            ClientEvent::UserDirectoryUpdated {
                                                user_id,
                                                username,
                                            } => UiEvent::SenderDirectoryUpdated {
                                                user_id,
                                                username,
                                            },
                                            ClientEvent::MessageDecrypted {
                                                message,
                                                plaintext,
                                            } => UiEvent::MessageDecrypted { message, plaintext },
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
                                                UiError::from_message(
                                                    UiErrorContext::DecryptMessage,
                                                    err,
                                                ),
                                            ),
                                        };
                                        let _ = ui_tx_clone.try_send(evt);
                                    }
                                });
                            }
                            let _ = ui_tx.try_send(UiEvent::LoginOk);
                        }
                        Err(err) => {
                            let _ = ui_tx.try_send(UiEvent::Error(UiError::from_message(
                                UiErrorContext::Login,
                                err.to_string(),
                            )));
                        }
                    },
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
