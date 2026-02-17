use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    thread,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use client_core::{ClientEvent, ClientHandle, PassthroughCrypto, RealtimeClient};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use eframe::egui;
use mime_guess::MimeGuess;
use rfd::FileDialog;
use shared::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId, Role},
    protocol::{
        ChannelSummary, GuildSummary, MemberSummary, MessageAttachment, MessageContent,
        MessagePayload, ServerEvent,
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
    },
    UploadAttachment {
        upload_id: u64,
        file_path: PathBuf,
        display_name: String,
        mime_type: Option<String>,
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    RetryUpload,
    CancelUpload,
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
    SenderDirectoryUpdated {
        user_id: i64,
        username: String,
    },
    Error(String),
    UploadState(AttachmentUploadUiState),
    AttachmentUploaded {
        upload_id: u64,
        file_id: FileId,
        file_name: String,
    },
    Server(ServerEvent),
}

#[derive(Debug, Clone)]
enum AttachmentUploadUiState {
    Idle,
    InProgress { file_name: String, progress: f32 },
    Failed { file_name: String, error: String },
    Completed { file_name: String },
}

#[derive(Debug, Clone)]
struct PendingAttachment {
    upload_id: u64,
    file_name: String,
    mime_type: Option<String>,
    file_id: Option<FileId>,
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
    guilds: Vec<GuildSummary>,
    channels: Vec<ChannelSummary>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,
    messages: HashMap<ChannelId, Vec<MessagePayload>>,
    members: HashMap<GuildId, Vec<MemberSummary>>,
    message_ids: HashMap<ChannelId, HashSet<MessageId>>,
    status: String,
    sender_directory: HashMap<i64, String>,
    attachment_upload_state: AttachmentUploadUiState,
    pending_attachment: Option<PendingAttachment>,
    next_upload_id: u64,
    settings_open: bool,
    view_state: AppViewState,
    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppViewState {
    Login,
    Main,
}

impl DesktopGuiApp {
    fn new(cmd_tx: Sender<BackendCommand>, ui_rx: Receiver<UiEvent>) -> Self {
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
            guilds: Vec::new(),
            channels: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            messages: HashMap::new(),
            members: HashMap::new(),
            message_ids: HashMap::new(),
            status: "Not logged in".to_string(),
            sender_directory: HashMap::new(),
            attachment_upload_state: AttachmentUploadUiState::Idle,
            pending_attachment: None,
            next_upload_id: 1,
            settings_open: false,
            view_state: AppViewState::Login,
            theme: ThemeSettings::discord_default(),
            applied_theme: None,
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
                    self.pending_attachment = None;
                    self.attachment_upload_state = AttachmentUploadUiState::Idle;
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
                UiEvent::UploadState(upload_state) => {
                    self.attachment_upload_state = upload_state.clone();
                    match upload_state {
                        AttachmentUploadUiState::Idle => {}
                        AttachmentUploadUiState::InProgress {
                            file_name,
                            progress,
                        } => {
                            self.status = format!(
                                "Uploading {file_name}: {:.0}%",
                                (progress * 100.0).clamp(0.0, 100.0)
                            );
                        }
                        AttachmentUploadUiState::Failed { file_name, error } => {
                            self.status = format!("Upload failed for {file_name}: {error}");
                        }
                        AttachmentUploadUiState::Completed { file_name } => {
                            self.status = format!("Attachment {file_name} uploaded");
                        }
                    }
                }
                UiEvent::AttachmentUploaded {
                    upload_id,
                    file_id,
                    file_name,
                } => {
                    if let Some(attachment) = &mut self.pending_attachment {
                        if attachment.upload_id == upload_id && attachment.file_name == file_name {
                            attachment.file_id = Some(file_id);
                        }
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
                            self.pending_attachment = None;
                            self.attachment_upload_state = AttachmentUploadUiState::Idle;
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
                                self.pending_attachment = None;
                                self.attachment_upload_state = AttachmentUploadUiState::Idle;
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
        if self.applied_theme == Some(self.theme) {
            return;
        }

        let mut style = (*ctx.style()).clone();
        style.visuals = visuals_for_theme(self.theme);
        ctx.set_style(style);
        self.applied_theme = Some(self.theme);
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

                if ui.button("Reset to Discord default").clicked() {
                    self.theme = ThemeSettings::discord_default();
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
                            .rounding(egui::Rounding::same(6.0))
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
                            .rounding(egui::Rounding::same(6.0))
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
                    if let Some(attachment) = self.pending_attachment.clone() {
                        ui.horizontal(|ui| {
                            let mut label = format!("📎 {}", attachment.file_name);
                            if attachment.file_id.is_some() {
                                label.push_str(" ✅");
                            }
                            ui.label(label);

                            match &self.attachment_upload_state {
                                AttachmentUploadUiState::InProgress { .. } => {
                                    if ui.small_button("Cancel").clicked() {
                                        queue_command(
                                            &self.cmd_tx,
                                            BackendCommand::CancelUpload,
                                            &mut self.status,
                                        );
                                    }
                                }
                                AttachmentUploadUiState::Failed { .. } => {
                                    if ui.small_button("Retry").clicked() {
                                        queue_command(
                                            &self.cmd_tx,
                                            BackendCommand::RetryUpload,
                                            &mut self.status,
                                        );
                                    }
                                }
                                _ => {}
                            }

                            if ui.small_button("Clear").clicked() {
                                self.pending_attachment = None;
                                self.attachment_upload_state = AttachmentUploadUiState::Idle;
                            }
                        });
                        ui.add_space(4.0);
                    }

                    ui.horizontal(|ui| {
                        let text_width = (ui.available_width() - 180.0).max(180.0);
                        let response = ui.add_sized(
                            [text_width, 72.0],
                            egui::TextEdit::multiline(&mut self.composer).hint_text(
                                "Message #channel (Enter to send, Shift+Enter for newline)",
                            ),
                        );
                        let attach_clicked = ui
                            .add_sized([80.0, 72.0], egui::Button::new("📎 Attach"))
                            .clicked();
                        let send_shortcut = response.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        let clicked_send = ui
                            .add_sized([80.0, 72.0], egui::Button::new("⬆ Send"))
                            .clicked();
                        if attach_clicked {
                            if let (Some(guild_id), Some(channel_id)) =
                                (self.selected_guild, self.selected_channel)
                            {
                                if let Some(file_path) = FileDialog::new().pick_file() {
                                    if let Some(display_name_os) = file_path.file_name() {
                                        let display_name =
                                            display_name_os.to_string_lossy().to_string();
                                        let mime_type = MimeGuess::from_path(&file_path)
                                            .first_raw()
                                            .map(ToString::to_string);
                                        let upload_id = self.next_upload_id;
                                        self.next_upload_id += 1;
                                        self.pending_attachment = Some(PendingAttachment {
                                            upload_id,
                                            file_name: display_name.clone(),
                                            mime_type: mime_type.clone(),
                                            file_id: None,
                                        });
                                        self.attachment_upload_state =
                                            AttachmentUploadUiState::InProgress {
                                                file_name: display_name.clone(),
                                                progress: 0.0,
                                            };
                                        queue_command(
                                            &self.cmd_tx,
                                            BackendCommand::UploadAttachment {
                                                upload_id,
                                                file_path,
                                                display_name,
                                                mime_type,
                                                guild_id,
                                                channel_id,
                                            },
                                            &mut self.status,
                                        );
                                    }
                                }
                            }
                        }

                        let has_text =
                            !self.composer.trim().is_empty() || self.pending_attachment.is_some();
                        if (send_shortcut || clicked_send) && has_text {
                            let text = self.composer.trim_end_matches('\n').to_string();
                            if let Some(pending) = &self.pending_attachment {
                                if pending.file_id.is_none() {
                                    self.status =
                                        "Wait for upload to finish before sending".to_string();
                                    return;
                                }
                            }

                            let payload = if let Some(pending) = self.pending_attachment.take() {
                                MessageContent {
                                    text,
                                    attachments: vec![MessageAttachment {
                                        file_id: pending.file_id.expect("checked above"),
                                        file_name: pending.file_name,
                                        mime_type: pending.mime_type,
                                    }],
                                }
                            } else {
                                MessageContent {
                                    text,
                                    attachments: Vec::new(),
                                }
                            };

                            let encoded = serde_json::to_string(&payload)
                                .unwrap_or_else(|_| payload.text.clone());
                            self.composer.clear();
                            self.attachment_upload_state = AttachmentUploadUiState::Idle;
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::SendMessage { text: encoded },
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
                            let parsed_content =
                                serde_json::from_str::<MessageContent>(&decoded).ok();
                            let decoded_text = parsed_content
                                .as_ref()
                                .map(|content| content.text.as_str())
                                .unwrap_or(decoded.as_str());
                            let sender_display = msg
                                .sender_username
                                .clone()
                                .or_else(|| self.sender_directory.get(&msg.sender_id.0).cloned())
                                .unwrap_or_else(|| msg.sender_id.0.to_string());
                            let sent_at = msg.sent_at.format("%Y-%m-%d %H:%M:%S UTC").to_string();

                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(sender_display).strong());
                                ui.label(egui::RichText::new(sent_at).small().weak());
                            });
                            ui.label(decoded_text);
                            if let Some(content) = parsed_content {
                                for attachment in content.attachments {
                                    ui.horizontal(|ui| {
                                        ui.weak("Attachment:");
                                        ui.label(format!(
                                            "📎 {} ({})",
                                            attachment.file_name, attachment.file_id.0
                                        ));
                                    });
                                }
                            }
                            ui.add_space(6.0);
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
}

fn spawn_backend_thread(cmd_rx: Receiver<BackendCommand>, ui_tx: Sender<UiEvent>) {
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        runtime.block_on(async move {
            let client = RealtimeClient::new(PassthroughCrypto);
            let mut current_upload_task: Option<tokio::task::JoinHandle<()>> = None;
            let mut last_upload_request: Option<(
                u64,
                PathBuf,
                String,
                Option<String>,
                GuildId,
                ChannelId,
            )> = None;

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
                    BackendCommand::SendMessage { text } => {
                        if let Err(err) = client.send_message(&text).await {
                            let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                        }
                    }
                    BackendCommand::UploadAttachment {
                        upload_id,
                        file_path,
                        display_name,
                        mime_type,
                        guild_id,
                        channel_id,
                    } => {
                        if let Some(handle) = current_upload_task.take() {
                            handle.abort();
                        }
                        last_upload_request = Some((
                            upload_id,
                            file_path.clone(),
                            display_name.clone(),
                            mime_type.clone(),
                            guild_id,
                            channel_id,
                        ));

                        let client = client.clone();
                        let ui_tx_upload = ui_tx.clone();
                        current_upload_task = Some(tokio::spawn(async move {
                            let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                AttachmentUploadUiState::InProgress {
                                    file_name: display_name.clone(),
                                    progress: 0.15,
                                },
                            ));
                            match tokio::fs::read(&file_path).await {
                                Ok(data) => {
                                    let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                        AttachmentUploadUiState::InProgress {
                                            file_name: display_name.clone(),
                                            progress: 0.55,
                                        },
                                    ));
                                    match client
                                        .upload_file(
                                            guild_id,
                                            channel_id,
                                            &display_name,
                                            mime_type.as_deref(),
                                            data,
                                        )
                                        .await
                                    {
                                        Ok(file_id) => {
                                            let _ = ui_tx_upload.try_send(
                                                UiEvent::AttachmentUploaded {
                                                    upload_id,
                                                    file_id,
                                                    file_name: display_name.clone(),
                                                },
                                            );
                                            let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Completed {
                                                    file_name: display_name,
                                                },
                                            ));
                                        }
                                        Err(err) => {
                                            let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Failed {
                                                    file_name: display_name,
                                                    error: err.to_string(),
                                                },
                                            ));
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                        AttachmentUploadUiState::Failed {
                                            file_name: display_name,
                                            error: err.to_string(),
                                        },
                                    ));
                                }
                            }
                        }));
                    }
                    BackendCommand::RetryUpload => {
                        if let Some((
                            upload_id,
                            file_path,
                            display_name,
                            mime_type,
                            guild_id,
                            channel_id,
                        )) = last_upload_request.clone()
                        {
                            if let Some(handle) = current_upload_task.take() {
                                handle.abort();
                            }
                            let client = client.clone();
                            let ui_tx_upload = ui_tx.clone();
                            current_upload_task = Some(tokio::spawn(async move {
                                let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                    AttachmentUploadUiState::InProgress {
                                        file_name: display_name.clone(),
                                        progress: 0.1,
                                    },
                                ));
                                match tokio::fs::read(&file_path).await {
                                    Ok(data) => match client
                                        .upload_file(
                                            guild_id,
                                            channel_id,
                                            &display_name,
                                            mime_type.as_deref(),
                                            data,
                                        )
                                        .await
                                    {
                                        Ok(file_id) => {
                                            let _ = ui_tx_upload.try_send(
                                                UiEvent::AttachmentUploaded {
                                                    upload_id,
                                                    file_id,
                                                    file_name: display_name.clone(),
                                                },
                                            );
                                            let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Completed {
                                                    file_name: display_name,
                                                },
                                            ));
                                        }
                                        Err(err) => {
                                            let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Failed {
                                                    file_name: display_name,
                                                    error: err.to_string(),
                                                },
                                            ));
                                        }
                                    },
                                    Err(err) => {
                                        let _ = ui_tx_upload.try_send(UiEvent::UploadState(
                                            AttachmentUploadUiState::Failed {
                                                file_name: display_name,
                                                error: err.to_string(),
                                            },
                                        ));
                                    }
                                }
                            }));
                        }
                    }
                    BackendCommand::CancelUpload => {
                        if let Some(handle) = current_upload_task.take() {
                            handle.abort();
                            let _ =
                                ui_tx.try_send(UiEvent::UploadState(AttachmentUploadUiState::Idle));
                            let _ = ui_tx
                                .try_send(UiEvent::Info("Attachment upload canceled".to_string()));
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
        Box::new(|_cc| Ok(Box::new(DesktopGuiApp::new(cmd_tx, ui_rx)))),
    )
}
