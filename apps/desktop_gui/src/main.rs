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
    AttachmentUpload, ClientEvent, ClientHandle, PassthroughCrypto, RealtimeClient,
    VoiceConnectOptions, VoiceParticipantState, VoiceSessionSnapshot,
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
        password_or_invite: String,
        password: Option<String>,
        token: Option<String>,
        remember_me: Option<bool>,
        auth_action: AuthAction,
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
    Error(String),
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
enum AuthAction {
    SignIn,
    CreateAccount,
}

impl AuthAction {
    fn label(self) -> &'static str {
        match self {
            AuthAction::SignIn => "Sign in",
            AuthAction::CreateAccount => "Create account",
        }
    }
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
            panel_rounding: 6,
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

struct DesktopGuiApp {
    cmd_tx: Sender<BackendCommand>,
    ui_rx: Receiver<UiEvent>,
    server_url: String,
    username: String,
    password_or_invite: String,
    password: String,
    auth_token: String,
    remember_me: bool,
    auth_action: AuthAction,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppViewState {
    Login,
    Main,
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
            password: String::new(),
            auth_token: String::new(),
            remember_me: false,
            auth_action: AuthAction::SignIn,
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
        }
    }

    fn process_ui_events(&mut self) {
        while let Ok(event) = self.ui_rx.try_recv() {
            match event {
                UiEvent::LoginOk => {
                    self.auth_session_established = true;
                    self.view_state = AppViewState::Main;
                    self.status = "Logged in - syncing guilds".to_string();
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
                    self.auth_session_established = false;
                    self.status = format!("Error: {err}");
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
        if self.readability.compact_density {
            style.spacing.item_spacing = egui::vec2(6.0, 4.0);
            style.spacing.button_padding = egui::vec2(6.0, 4.0);
            style.spacing.interact_size.y = 22.0;
        } else {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(8.0, 5.0);
            style.spacing.interact_size.y = 28.0;
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

    fn show_login_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(50.0);
                ui.heading("Realtime Chat Login");
                ui.add_space(16.0);

                egui::Grid::new("login_form")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Server:");
                        ui.text_edit_singleline(&mut self.server_url);
                        ui.end_row();

                        ui.label("Username:");
                        ui.text_edit_singleline(&mut self.username);
                        ui.end_row();

                        ui.label("Invite/legacy password:");
                        ui.text_edit_singleline(&mut self.password_or_invite);
                        ui.end_row();

                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                        ui.end_row();

                        ui.label("Token:");
                        ui.text_edit_singleline(&mut self.auth_token);
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.auth_action,
                        AuthAction::SignIn,
                        AuthAction::SignIn.label(),
                    );
                    ui.selectable_value(
                        &mut self.auth_action,
                        AuthAction::CreateAccount,
                        AuthAction::CreateAccount.label(),
                    );
                    ui.checkbox(&mut self.remember_me, "Remember me");
                });

                ui.add_space(12.0);
                if ui.button(self.auth_action.label()).clicked() {
                    self.auth_session_established = false;
                    queue_command(
                        &self.cmd_tx,
                        BackendCommand::Login {
                            server_url: self.server_url.clone(),
                            username: self.username.clone(),
                            password_or_invite: self.password_or_invite.clone(),
                            password: (!self.password.trim().is_empty())
                                .then(|| self.password.clone()),
                            token: (!self.auth_token.trim().is_empty())
                                .then(|| self.auth_token.clone()),
                            remember_me: self.remember_me.then_some(true),
                            auth_action: self.auth_action,
                        },
                        &mut self.status,
                    );
                }

                ui.add_space(8.0);
                ui.separator();
                ui.label("Have an invite code? Join a guild after you sign in.");

                ui.horizontal(|ui| {
                    ui.label("Invite code:");
                    ui.text_edit_singleline(&mut self.password_or_invite);
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

                ui.add_space(10.0);
                ui.label(&self.status);
            });
        });
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

    fn show_main_workspace(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            let top_bar_bg = theme_discord_dark_palette(self.theme)
                .map(|p| p.navigation_background)
                .unwrap_or(ui.visuals().panel_fill);
            egui::Frame::none().fill(top_bar_bg).show(ui, |ui| {
                egui::menu::bar(ui, |ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.settings_open = true;
                    }

                    ui.menu_button("Account", |ui| {
                        ui.label("Signed in as current user");
                    });
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("Server:");
                    ui.text_edit_singleline(&mut self.server_url);
                    ui.label("Username:");
                    ui.text_edit_singleline(&mut self.username);
                    ui.label("Invite (legacy):");
                    ui.text_edit_singleline(&mut self.password_or_invite);
                });

                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.auth_action,
                        AuthAction::SignIn,
                        AuthAction::SignIn.label(),
                    );
                    ui.selectable_value(
                        &mut self.auth_action,
                        AuthAction::CreateAccount,
                        AuthAction::CreateAccount.label(),
                    );
                    ui.separator();
                    ui.label("Password:");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                    ui.label("Token:");
                    ui.add(egui::TextEdit::singleline(&mut self.auth_token));
                    ui.checkbox(&mut self.remember_me, "Remember me");

                    if ui.button(self.auth_action.label()).clicked() {
                        self.auth_session_established = false;
                        queue_command(
                            &self.cmd_tx,
                            BackendCommand::Login {
                                server_url: self.server_url.clone(),
                                username: self.username.clone(),
                                password_or_invite: self.password_or_invite.clone(),
                                password: (!self.password.trim().is_empty())
                                    .then(|| self.password.clone()),
                                token: (!self.auth_token.trim().is_empty())
                                    .then(|| self.auth_token.clone()),
                                remember_me: self.remember_me.then_some(true),
                                auth_action: self.auth_action,
                            },
                            &mut self.status,
                        );
                    }

                    let constrained = ui.available_width() < 220.0;
                    if constrained {
                        ui.menu_button("⋯ More", |ui| {
                            if ui.button("Refresh Guilds").clicked() {
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::ListGuilds,
                                    &mut self.status,
                                );
                                ui.close_menu();
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
                                ui.close_menu();
                            }
                        });
                    } else {
                        if ui.button("🔄").on_hover_text("Refresh Guilds").clicked() {
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::ListGuilds,
                                &mut self.status,
                            );
                        }

                        if ui.button("➕").on_hover_text("Join Invite").clicked() {
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
                    }
                });

                if self.password.trim().is_empty() && self.auth_token.trim().is_empty() {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Legacy username-only mode active. Password/token auth is reserved for upcoming secure auth backend work.",
                    );
                }
                ui.label(&self.status);
            });
        });

        self.show_settings_window(ctx);

        egui::SidePanel::left("guilds_panel")
            .default_width(140.0)
            .show(ctx, |ui| {
                let nav_bg = theme_discord_dark_palette(self.theme)
                    .map(|p| p.navigation_background)
                    .unwrap_or(ui.visuals().panel_fill);
                egui::Frame::none().fill(nav_bg).show(ui, |ui| {
                    ui.heading("Guilds");
                    ui.add_enabled_ui(self.auth_session_established, |ui| {
                        if let Some(guild_id) = self.selected_guild {
                            if ui.button("Create Invite").clicked() {
                                queue_command(
                                    &self.cmd_tx,
                                    BackendCommand::CreateInvite { guild_id },
                                    &mut self.status,
                                );
                            }
                        }

                        for guild in &self.guilds {
                            let selected = self.selected_guild == Some(guild.guild_id);
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
                            let row_stroke = if selected {
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
                            };

                            let text_color = if selected {
                                discord_dark
                                    .map(|p| p.guild_text_highlighted)
                                    .unwrap_or(ui.visuals().strong_text_color())
                            } else {
                                discord_dark
                                    .map(|p| p.guild_text_unhighlighted)
                                    .unwrap_or(ui.visuals().text_color())
                            };

                            let response = egui::Frame::none()
                                .fill(if selected { selected_bg } else { base_bg })
                                .stroke(row_stroke)
                                .rounding(egui::Rounding::same(f32::from(
                                    self.theme.panel_rounding,
                                )))
                                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                .show(ui, |ui| {
                                    ui.selectable_label(
                                        selected,
                                        egui::RichText::new(&guild.name).color(text_color),
                                    )
                                })
                                .inner;
                            if response.hovered() && !selected {
                                if let Some(palette) = discord_dark {
                                    ui.ctx()
                                        .layer_painter(egui::LayerId::new(
                                            egui::Order::Background,
                                            egui::Id::new("guild_hover_row"),
                                        ))
                                        .rect_filled(
                                            response.rect,
                                            egui::Rounding::same(f32::from(
                                                self.theme.panel_rounding,
                                            )),
                                            palette.guild_entry_hover,
                                        );
                                }
                            }
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
                            ui.add_space(4.0);
                        }
                    });

                    if !self.auth_session_established {
                        ui.separator();
                        ui.label("Sign in or create an account to access guilds.");
                    }
                });
            });

        egui::SidePanel::left("channels_panel")
            .default_width(220.0)
            .show(ctx, |ui| {
                let nav_bg = theme_discord_dark_palette(self.theme)
                    .map(|p| p.navigation_background)
                    .unwrap_or(ui.visuals().panel_fill);
                egui::Frame::none().fill(nav_bg).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Channels");
                        if ui.button("Refresh").clicked() {
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
                            let row_fill = if selected { selected_bg } else { base_bg };
                            let row_stroke = if selected {
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
                            };
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

                            let response = egui::Frame::none()
                                .fill(row_fill)
                                .stroke(row_stroke)
                                .rounding(egui::Rounding::same(f32::from(
                                    self.theme.panel_rounding,
                                )))
                                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [18.0, 18.0],
                                            egui::Label::new(egui::RichText::new(icon).monospace()),
                                        );
                                        let _ = ui.selectable_label(selected, &channel.name);
                                        if let Some(count) = occupancy {
                                            ui.label(format!("({count})"));
                                            if let Some(hint) = connection_hint {
                                                ui.small(hint);
                                            }
                                        }
                                    })
                                    .response
                                })
                                .inner;
                            if response.hovered() && !selected {
                                if let Some(palette) = discord_dark {
                                    ui.ctx()
                                        .layer_painter(egui::LayerId::new(
                                            egui::Order::Background,
                                            egui::Id::new("channel_hover_row"),
                                        ))
                                        .rect_filled(
                                            response.rect,
                                            egui::Rounding::same(f32::from(
                                                self.theme.panel_rounding,
                                            )),
                                            palette.guild_entry_hover,
                                        );
                                }
                            }
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
                            ui.add_space(4.0);
                        }

                        if let Some(channel) = self.selected_voice_channel().cloned() {
                            ui.separator();
                            let connected_here =
                                self.voice_ui.is_channel_connected(channel.channel_id);
                            let cta_label = if connected_here {
                                "Leave voice"
                            } else {
                                "Join voice"
                            };
                            if ui.button(cta_label).clicked() {
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
                        ui.label("Sign in to browse channels.");
                    }
                });
            });

        egui::SidePanel::right("members_panel")
            .default_width(220.0)
            .show(ctx, |ui| {
                let members_bg = theme_discord_dark_palette(self.theme)
                    .map(|p| p.members_background)
                    .unwrap_or(ui.visuals().panel_fill);
                egui::Frame::none().fill(members_bg).show(ui, |ui| {
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
                    ui.heading("Voice");
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
                        ui.colored_label(egui::Color32::RED, format!("Last voice error: {err}"));
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
                                    [ui.available_width() - 130.0, 72.0],
                                    egui::TextEdit::multiline(&mut self.composer).hint_text(
                                        "Message #channel (Enter to send, Shift+Enter for newline)",
                                    ),
                                )
                            })
                            .inner;
                        let send_shortcut = response.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        let clicked_send = ui
                            .add_sized([80.0, 72.0], egui::Button::new("⬆ Send"))
                            .clicked();
                        let has_text = !self.composer.trim().is_empty();
                        let has_attachment = self.pending_attachment.is_some();
                        if (send_shortcut || clicked_send) && (has_text || has_attachment) {
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

                    ui.horizontal(|ui| {
                        if ui.button("📎").on_hover_text("Attach file").clicked() {
                            self.pending_attachment = rfd::FileDialog::new().pick_file();
                        }
                        let response = ui.add_sized(
                            [ui.available_width() - 130.0, 72.0],
                            egui::TextEdit::multiline(&mut self.composer).hint_text(
                                "Message #channel (Enter to send, Shift+Enter for newline)",
                            ),
                        );
                        let send_shortcut = response.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        let clicked_send = ui
                            .add_sized([80.0, 72.0], egui::Button::new("⬆ Send"))
                            .clicked();
                        let has_text = !self.composer.trim().is_empty();
                        let has_attachment = self.pending_attachment.is_some();
                        if (send_shortcut || clicked_send) && (has_text || has_attachment) {
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
                    });
                });
                if !can_send {
                    ui.centered_and_justified(|ui| {
                        ui.weak("Pick a channel to start chatting.");
                    });
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let message_panel_bg = theme_discord_dark_palette(self.theme)
                .map(|p| p.message_background)
                .unwrap_or(ui.visuals().panel_fill);
            egui::Frame::none().fill(message_panel_bg).show(ui, |ui| {
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
                                    egui::Margin::symmetric(6.0, 4.0)
                                } else {
                                    egui::Margin::symmetric(8.0, 6.0)
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
                                    4.0
                                } else {
                                    6.0
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
                    let max_width = (ui.available_width() * 0.75).clamp(120.0, 360.0);
                    let mut preview_size = texture.size_vec2();
                    if preview_size.x > max_width {
                        preview_size *= max_width / preview_size.x;
                    }
                    preview_size.y = preview_size.y.min(240.0);
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
    match cmd_tx.try_send(cmd) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            *status = "UI command queue is full; please retry".to_string();
        }
        Err(TrySendError::Disconnected(_)) => {
            *status = "Backend is unavailable".to_string();
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
    guild_text_unhighlighted: egui::Color32,
    guild_text_highlighted: egui::Color32,
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
        guild_text_unhighlighted: egui::Color32::from_rgb(129, 130, 138),
        guild_text_highlighted: egui::Color32::from_rgb(251, 251, 251),
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

fn spawn_backend_thread(cmd_rx: Receiver<BackendCommand>, ui_tx: Sender<UiEvent>) {
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        runtime.block_on(async move {
            let client = RealtimeClient::new(PassthroughCrypto);

            let mut subscribed = false;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BackendCommand::Login {
                        server_url,
                        username,
                        password_or_invite,
                        password,
                        token,
                        remember_me: _remember_me,
                        auth_action: _auth_action,
                    } => {
                        let credential = password
                            .as_deref()
                            .filter(|value| !value.trim().is_empty())
                            .or_else(|| token.as_deref().filter(|value| !value.trim().is_empty()))
                            .unwrap_or(password_or_invite.as_str());
                        match client.login(&server_url, &username, credential).await {
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
                                                } => {
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
                                                ClientEvent::Error(err) => UiEvent::Error(err),
                                            };
                                            let _ = ui_tx_clone.try_send(evt);
                                        }
                                    });
                                }
                                let _ = ui_tx.try_send(UiEvent::LoginOk);
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                            }
                        }
                    }
                    BackendCommand::ListGuilds => {
                        if let Err(err) = client.list_guilds().await {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::ListChannels { guild_id } => {
                        if let Err(err) = client.list_channels(guild_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::ListMembers { guild_id } => {
                        if let Err(err) = client.list_members(guild_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::SelectChannel { channel_id } => {
                        if let Err(err) = client.select_channel(channel_id).await {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::LoadMoreMessages { channel_id, before } => {
                        if let Err(err) = client.fetch_messages(channel_id, 100, Some(before)).await
                        {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
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
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::FetchAttachmentPreview { file_id } => {
                        match client.download_file(file_id).await {
                            Ok(bytes) => match decode_preview_image(&bytes) {
                                Ok(image) => {
                                    let _ = ui_tx.try_send(UiEvent::AttachmentPreviewLoaded {
                                        file_id,
                                        image,
                                        original_bytes: bytes,
                                    });
                                }
                                Err(err) => {
                                    let _ = ui_tx.try_send(UiEvent::AttachmentPreviewFailed {
                                        file_id,
                                        reason: err,
                                    });
                                }
                            },
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
                                            let _ = ui_tx.try_send(UiEvent::Error(format!(
                                                "Failed to save attachment: {err}"
                                            )));
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(format!(
                                    "Failed to download attachment: {err}"
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
                                let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
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
                                    let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                                }
                            }
                            Err(err) => {
                                let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                            }
                        }
                    }
                    BackendCommand::ConnectVoice {
                        guild_id,
                        channel_id,
                    } => {
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
    use super::{human_readable_bytes, DisplayMessage};
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
