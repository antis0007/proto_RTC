use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    thread,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use client_core::{AttachmentUpload, ClientEvent, ClientHandle, PassthroughCrypto, RealtimeClient};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use eframe::egui;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, ChannelKind, GuildId, MessageId, Role},
    protocol::{ChannelSummary, GuildSummary, MemberSummary, MessagePayload, ServerEvent},
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
        file_id: shared::domain::FileId,
        filename: String,
    },
    CreateInvite {
        guild_id: GuildId,
    },
    JoinWithInvite {
        invite_code: String,
    },
}

enum UiEvent {
    LoginOk,
    Info(String),
    InviteCreated(String),
    SenderDirectoryUpdated { user_id: i64, username: String },
    Error(String),
    Server(ServerEvent),
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
    AtomOneDark,
    EguiLight,
}

impl ThemePreset {
    fn label(self) -> &'static str {
        match self {
            ThemePreset::DiscordDark => "Discord (Dark)",
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
    AtomOneDark,
    EguiLight,
}

impl From<ThemePreset> for PersistedThemePreset {
    fn from(value: ThemePreset) -> Self {
        match value {
            ThemePreset::DiscordDark => Self::DiscordDark,
            ThemePreset::AtomOneDark => Self::AtomOneDark,
            ThemePreset::EguiLight => Self::EguiLight,
        }
    }
}

impl From<PersistedThemePreset> for ThemePreset {
    fn from(value: PersistedThemePreset) -> Self {
        match value {
            PersistedThemePreset::DiscordDark => Self::DiscordDark,
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
    guilds: Vec<GuildSummary>,
    channels: Vec<ChannelSummary>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,
    messages: HashMap<ChannelId, Vec<MessagePayload>>,
    members: HashMap<GuildId, Vec<MemberSummary>>,
    message_ids: HashMap<ChannelId, HashSet<MessageId>>,
    status: String,
    sender_directory: HashMap<i64, String>,
    settings_open: bool,
    view_state: AppViewState,
    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
    readability: UiReadabilitySettings,
    applied_readability: Option<UiReadabilitySettings>,
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
            guilds: Vec::new(),
            channels: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            messages: HashMap::new(),
            members: HashMap::new(),
            message_ids: HashMap::new(),
            status: "Not logged in".to_string(),
            sender_directory: HashMap::new(),
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
                    ServerEvent::MessageReceived { message } => {
                        if let Some(username) = &message.sender_username {
                            self.sender_directory
                                .insert(message.sender_id.0, username.clone());
                        }

                        let ids = self.message_ids.entry(message.channel_id).or_default();
                        if ids.insert(message.message_id) {
                            let messages = self.messages.entry(message.channel_id).or_default();
                            messages.push(message);
                            messages.sort_by_key(|m| m.message_id.0);
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
            .map(|message| message.message_id)
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

    fn show_main_workspace(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
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
                            queue_command(&self.cmd_tx, BackendCommand::ListGuilds, &mut self.status);
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
                        queue_command(&self.cmd_tx, BackendCommand::ListGuilds, &mut self.status);
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

        self.show_settings_window(ctx);

        egui::SidePanel::left("guilds_panel")
            .default_width(140.0)
            .show(ctx, |ui| {
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
                        let base_bg = if self.theme.list_row_shading {
                            ui.visuals().faint_bg_color
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let selected_bg = ui.visuals().selection.bg_fill.gamma_multiply(
                            if self.theme.list_row_shading {
                                0.35
                            } else {
                                0.22
                            },
                        );
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

                        let response = egui::Frame::none()
                            .fill(row_fill)
                            .stroke(row_stroke)
                            .rounding(egui::Rounding::same(f32::from(self.theme.panel_rounding)))
                            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                            .show(ui, |ui| ui.selectable_label(selected, &guild.name))
                            .inner;
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

        egui::SidePanel::left("channels_panel")
            .default_width(220.0)
            .show(ctx, |ui| {
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
                        let base_bg = if self.theme.list_row_shading {
                            ui.visuals().faint_bg_color
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let selected_bg = ui.visuals().selection.bg_fill.gamma_multiply(
                            if self.theme.list_row_shading {
                                0.35
                            } else {
                                0.22
                            },
                        );
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

                        let response = egui::Frame::none()
                            .fill(row_fill)
                            .stroke(row_stroke)
                            .rounding(egui::Rounding::same(f32::from(self.theme.panel_rounding)))
                            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [18.0, 18.0],
                                        egui::Label::new(egui::RichText::new(icon).monospace()),
                                    );
                                    ui.selectable_label(selected, &channel.name)
                                })
                                .inner
                            })
                            .inner;
                        if response.clicked() {
                            self.selected_channel = Some(channel.channel_id);
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
                });

                if !self.auth_session_established {
                    ui.separator();
                    ui.label("Sign in to browse channels.");
                }
            });

        egui::SidePanel::right("members_panel")
            .default_width(220.0)
            .show(ctx, |ui| {
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
                                let mute_label = if member.muted { " · 🔇 muted" } else { "" };
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
                ui.label("No participants");
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
                    if let Some(path) = &self.pending_attachment {
                        ui.small(format!("Attached: {}", path.display()));
                    }
                });
                if !can_send {
                    ui.centered_and_justified(|ui| {
                        ui.weak("Pick a channel to start chatting.");
                    });
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
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
                    if let Some(messages) = self.messages.get(&channel_id) {
                        for msg in messages {
                            let decoded = STANDARD
                                .decode(msg.ciphertext_b64.as_bytes())
                                .ok()
                                .and_then(|bytes| String::from_utf8(bytes).ok())
                                .unwrap_or_else(|| "<binary message>".to_string());
                            let sender_display = msg
                                .sender_username
                                .clone()
                                .or_else(|| self.sender_directory.get(&msg.sender_id.0).cloned())
                                .unwrap_or_else(|| msg.sender_id.0.to_string());
                            let sent_at = msg.sent_at.format("%Y-%m-%d %H:%M:%S UTC").to_string();

                            let message_margin = if self.readability.compact_density {
                                egui::Margin::symmetric(6.0, 4.0)
                            } else {
                                egui::Margin::symmetric(8.0, 6.0)
                            };
                            let frame = if self.readability.message_bubble_backgrounds {
                                egui::Frame::none()
                                    .fill(ui.visuals().faint_bg_color.gamma_multiply(0.45))
                            } else {
                                egui::Frame::none()
                            };
                            frame
                                .rounding(egui::Rounding::same(f32::from(
                                    self.theme.panel_rounding,
                                )))
                                .inner_margin(message_margin)
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new(sender_display).strong());
                                        if self.readability.show_timestamps {
                                            ui.label(egui::RichText::new(sent_at).small().weak());
                                        }
                                    });
                                    ui.label(decoded);
                                    if let Some(attachment) = &msg.attachment {
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "📎 {} ({} bytes)",
                                                attachment.filename, attachment.size_bytes
                                            ));
                                            if ui.button("Download").clicked() {
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
                                });
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
                                ui.heading("No messages");
                                ui.weak("This conversation is quiet for now.");
                            },
                        );
                    }
                } else {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            ui.heading("Select a channel");
                            ui.weak("Choose a channel from the left to view and send messages.");
                        },
                    );
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
    }
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

fn visuals_for_theme(theme: ThemeSettings) -> egui::Visuals {
    let mut visuals = match theme.preset {
        ThemePreset::DiscordDark => {
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
