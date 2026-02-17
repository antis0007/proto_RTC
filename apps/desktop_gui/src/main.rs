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
    domain::{ChannelId, ChannelKind, FileId, GuildId, MessageId},
    protocol::{
        ChannelSummary, GuildSummary, MessageAttachment, MessageContent, MessagePayload,
        ServerEvent,
    },
};

#[derive(Clone)]
enum BackendCommand {
    Login {
        server_url: String,
        username: String,
        password_or_invite: String,
    },
    ListGuilds,
    ListChannels {
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
}

impl ThemeSettings {
    fn discord_default() -> Self {
        Self {
            preset: ThemePreset::DiscordDark,
            accent_color: egui::Color32::from_rgb(88, 101, 242),
            panel_rounding: 6,
        }
    }
}

struct DesktopGuiApp {
    cmd_tx: Sender<BackendCommand>,
    ui_rx: Receiver<UiEvent>,
    server_url: String,
    username: String,
    password_or_invite: String,
    composer: String,
    guilds: Vec<GuildSummary>,
    channels: Vec<ChannelSummary>,
    selected_guild: Option<GuildId>,
    selected_channel: Option<ChannelId>,
    messages: HashMap<ChannelId, Vec<MessagePayload>>,
    message_ids: HashMap<ChannelId, HashSet<MessageId>>,
    status: String,
    attachment_upload_state: AttachmentUploadUiState,
    pending_attachment: Option<PendingAttachment>,
    next_upload_id: u64,
    settings_open: bool,
    theme: ThemeSettings,
    applied_theme: Option<ThemeSettings>,
}

impl DesktopGuiApp {
    fn new(cmd_tx: Sender<BackendCommand>, ui_rx: Receiver<UiEvent>) -> Self {
        Self {
            cmd_tx,
            ui_rx,
            server_url: "http://127.0.0.1:8443".to_string(),
            username: "alice".to_string(),
            password_or_invite: String::new(),
            composer: String::new(),
            guilds: Vec::new(),
            channels: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            messages: HashMap::new(),
            message_ids: HashMap::new(),
            status: "Not logged in".to_string(),
            attachment_upload_state: AttachmentUploadUiState::Idle,
            pending_attachment: None,
            next_upload_id: 1,
            settings_open: false,
            theme: ThemeSettings::discord_default(),
            applied_theme: None,
        }
    }

    fn process_ui_events(&mut self) {
        while let Ok(event) = self.ui_rx.try_recv() {
            match event {
                UiEvent::LoginOk => {
                    self.status = "Logged in - syncing guilds".to_string();
                    self.guilds.clear();
                    self.channels.clear();
                    self.messages.clear();
                    self.message_ids.clear();
                    self.selected_guild = None;
                    self.selected_channel = None;
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
                UiEvent::Error(err) => {
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
                    ServerEvent::MessageReceived { message } => {
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

    fn decode_message_content(message: &MessagePayload) -> MessageContent {
        let decoded = STANDARD
            .decode(message.ciphertext_b64.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| "<binary message>".to_string());

        serde_json::from_str::<MessageContent>(&decoded).unwrap_or(MessageContent {
            text: decoded,
            attachments: Vec::new(),
        })
    }

    fn uploadable_channel_context(&self) -> Option<(GuildId, ChannelId)> {
        Some((self.selected_guild?, self.selected_channel?))
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

                if ui.button("Reset to Discord default").clicked() {
                    self.theme = ThemeSettings::discord_default();
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

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                if ui.button("⚙ Settings").clicked() {
                    self.settings_open = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label("Server:");
                ui.text_edit_singleline(&mut self.server_url);
                ui.label("Username:");
                ui.text_edit_singleline(&mut self.username);
                ui.label("Invite/Password:");
                ui.text_edit_singleline(&mut self.password_or_invite);
                if ui.button("Login").clicked() {
                    queue_command(
                        &self.cmd_tx,
                        BackendCommand::Login {
                            server_url: self.server_url.clone(),
                            username: self.username.clone(),
                            password_or_invite: self.password_or_invite.clone(),
                        },
                        &mut self.status,
                    );
                }
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
            ui.label(&self.status);
        });

        self.show_settings_window(ctx);

        egui::SidePanel::left("guilds_panel")
            .default_width(140.0)
            .show(ctx, |ui| {
                ui.heading("Guilds");
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
                    if ui.selectable_label(selected, &guild.name).clicked() {
                        self.selected_guild = Some(guild.guild_id);
                        self.selected_channel = None;
                        self.channels.clear();
                        queue_command(&self.cmd_tx, BackendCommand::CancelUpload, &mut self.status);
                        self.pending_attachment = None;
                        self.attachment_upload_state = AttachmentUploadUiState::Idle;
                        queue_command(
                            &self.cmd_tx,
                            BackendCommand::ListChannels {
                                guild_id: guild.guild_id,
                            },
                            &mut self.status,
                        );
                    }
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
                        } else {
                            self.status = "Select a guild first".to_string();
                        }
                    }
                });
                for channel in &self.channels {
                    let icon = match channel.kind {
                        ChannelKind::Text => "#",
                        ChannelKind::Voice => "🔊",
                    };
                    let label = format!("{icon} {}", channel.name);
                    let selected = self.selected_channel == Some(channel.channel_id);
                    if ui.selectable_label(selected, label).clicked() {
                        self.selected_channel = Some(channel.channel_id);
                        queue_command(&self.cmd_tx, BackendCommand::CancelUpload, &mut self.status);
                        self.pending_attachment = None;
                        self.attachment_upload_state = AttachmentUploadUiState::Idle;
                        queue_command(
                            &self.cmd_tx,
                            BackendCommand::SelectChannel {
                                channel_id: channel.channel_id,
                            },
                            &mut self.status,
                        );
                    }
                }
            });

        egui::SidePanel::right("members_panel")
            .default_width(160.0)
            .show(ctx, |ui| {
                ui.heading("Members");
                ui.label("(placeholder)");
                ui.separator();
                ui.heading("Voice");
                ui.label("No participants");
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
                            let content = Self::decode_message_content(msg);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("{}: {}", msg.sender_id.0, content.text));
                                for attachment in content.attachments {
                                    ui.label(format!(
                                        "📎 {} (file #{})",
                                        attachment.file_name, attachment.file_id.0
                                    ));
                                }
                            });
                        }
                    } else {
                        ui.label("No messages yet");
                    }
                } else {
                    ui.label("Select a channel");
                }
            });

            ui.separator();
            if let Some(attachment) = &self.pending_attachment {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Pending attachment: {}", attachment.file_name));
                    if let Some(file_id) = attachment.file_id {
                        ui.label(format!("uploaded as file #{}", file_id.0));
                    }
                });
            }

            match &self.attachment_upload_state {
                AttachmentUploadUiState::InProgress {
                    file_name,
                    progress,
                } => {
                    ui.label(format!(
                        "Uploading {file_name}: {:.0}%",
                        (progress * 100.0).clamp(0.0, 100.0)
                    ));
                }
                AttachmentUploadUiState::Failed { file_name, error } => {
                    ui.label(format!("Upload failed for {file_name}: {error}"));
                }
                AttachmentUploadUiState::Completed { file_name } => {
                    ui.label(format!("Upload completed: {file_name}"));
                }
                AttachmentUploadUiState::Idle => {}
            }

            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.composer)
                        .hint_text("Type a message and press Enter"),
                );
                if ui.button("Attach").clicked() {
                    if let Some((guild_id, channel_id)) = self.uploadable_channel_context() {
                        if let Some(path) = FileDialog::new().pick_file() {
                            let upload_id = self.next_upload_id;
                            self.next_upload_id = self.next_upload_id.saturating_add(1);
                            let display_name = path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("attachment")
                                .to_string();
                            let mime_type =
                                MimeGuess::from_path(&path).first_raw().map(str::to_string);
                            self.pending_attachment = Some(PendingAttachment {
                                upload_id,
                                file_name: display_name.clone(),
                                mime_type: mime_type.clone(),
                                file_id: None,
                            });
                            self.attachment_upload_state = AttachmentUploadUiState::InProgress {
                                file_name: display_name.clone(),
                                progress: 0.0,
                            };
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::UploadAttachment {
                                    upload_id,
                                    file_path: path,
                                    display_name,
                                    mime_type,
                                    guild_id,
                                    channel_id,
                                },
                                &mut self.status,
                            );
                        }
                    } else {
                        self.status = "Select a guild and channel before attaching files".into();
                    }
                }
                if matches!(
                    self.attachment_upload_state,
                    AttachmentUploadUiState::InProgress { .. }
                ) && ui.button("Cancel upload").clicked()
                {
                    queue_command(&self.cmd_tx, BackendCommand::CancelUpload, &mut self.status);
                }
                if matches!(
                    self.attachment_upload_state,
                    AttachmentUploadUiState::Failed { .. }
                ) && ui.button("Retry upload").clicked()
                {
                    queue_command(&self.cmd_tx, BackendCommand::RetryUpload, &mut self.status);
                }
                let pressed_enter =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked_send = ui.button("Send").clicked();
                let has_uploaded_attachment = self
                    .pending_attachment
                    .as_ref()
                    .and_then(|attachment| attachment.file_id)
                    .is_some();
                let should_send = (pressed_enter || clicked_send)
                    && (!self.composer.trim().is_empty() || has_uploaded_attachment);
                if should_send {
                    let attachments = self
                        .pending_attachment
                        .as_ref()
                        .and_then(|attachment| {
                            attachment.file_id.map(|file_id| MessageAttachment {
                                file_id,
                                file_name: attachment.file_name.clone(),
                                mime_type: attachment.mime_type.clone(),
                            })
                        })
                        .into_iter()
                        .collect::<Vec<_>>();
                    let payload = MessageContent {
                        text: std::mem::take(&mut self.composer),
                        attachments,
                    };
                    let text =
                        serde_json::to_string(&payload).unwrap_or_else(|_| payload.text.clone());
                    queue_command(
                        &self.cmd_tx,
                        BackendCommand::SendMessage { text },
                        &mut self.status,
                    );
                    self.pending_attachment = None;
                    self.attachment_upload_state = AttachmentUploadUiState::Idle;
                    response.request_focus();
                }
            });
        });

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
            let mut upload_task: Option<tokio::task::JoinHandle<()>> = None;
            let mut last_upload: Option<BackendCommand> = None;

            let mut subscribed = false;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BackendCommand::Login {
                        server_url,
                        username,
                        password_or_invite,
                    } => {
                        match client
                            .login(&server_url, &username, &password_or_invite)
                            .await
                        {
                            Ok(()) => {
                                if !subscribed {
                                    subscribed = true;
                                    let mut events = client.subscribe_events();
                                    let ui_tx_clone = ui_tx.clone();
                                    tokio::spawn(async move {
                                        while let Ok(event) = events.recv().await {
                                            let evt = match event {
                                                ClientEvent::Server(evt) => UiEvent::Server(evt),
                                                ClientEvent::Error(err) => UiEvent::Error(err),
                                            };
                                            let _ = ui_tx_clone.try_send(evt);
                                        }
                                    });
                                }
                                let invite_code = password_or_invite.trim().to_string();
                                if !invite_code.is_empty() {
                                    if let Err(err) = client.join_with_invite(&invite_code).await {
                                        let _ = ui_tx.try_send(UiEvent::Error(err.to_string()));
                                    }
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
                    BackendCommand::UploadAttachment {
                        upload_id,
                        file_path,
                        display_name,
                        mime_type,
                        guild_id,
                        channel_id,
                    } => {
                        if let Some(task) = upload_task.take() {
                            task.abort();
                        }

                        let retry_file_path = file_path.clone();
                        let retry_display_name = display_name.clone();
                        let retry_mime_type = mime_type.clone();

                        let _ = ui_tx.try_send(UiEvent::UploadState(
                            AttachmentUploadUiState::InProgress {
                                file_name: display_name.clone(),
                                progress: 0.05,
                            },
                        ));

                        let ui_tx_clone = ui_tx.clone();
                        let client_clone = client.clone();
                        let display_name_for_task = display_name.clone();
                        let mime_for_task = mime_type.clone();
                        upload_task = Some(tokio::spawn(async move {
                            match tokio::fs::read(&file_path).await {
                                Ok(file_bytes) => {
                                    let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                        AttachmentUploadUiState::InProgress {
                                            file_name: display_name_for_task.clone(),
                                            progress: 0.4,
                                        },
                                    ));
                                    match client_clone
                                        .upload_file(
                                            guild_id,
                                            channel_id,
                                            mime_for_task.as_deref(),
                                            file_bytes,
                                        )
                                        .await
                                    {
                                        Ok(file_id) => {
                                            let _ =
                                                ui_tx_clone.try_send(UiEvent::AttachmentUploaded {
                                                    upload_id,
                                                    file_id,
                                                    file_name: display_name_for_task.clone(),
                                                });
                                            let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Completed {
                                                    file_name: display_name_for_task,
                                                },
                                            ));
                                        }
                                        Err(err) => {
                                            let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Failed {
                                                    file_name: display_name_for_task,
                                                    error: err.to_string(),
                                                },
                                            ));
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                        AttachmentUploadUiState::Failed {
                                            file_name: display_name_for_task,
                                            error: format!("failed reading local file: {err}"),
                                        },
                                    ));
                                }
                            }
                        }));

                        last_upload = Some(BackendCommand::UploadAttachment {
                            upload_id,
                            file_path: retry_file_path,
                            display_name: retry_display_name,
                            mime_type: retry_mime_type,
                            guild_id,
                            channel_id,
                        });
                    }
                    BackendCommand::RetryUpload => {
                        if let Some(BackendCommand::UploadAttachment {
                            upload_id,
                            file_path,
                            display_name,
                            mime_type,
                            guild_id,
                            channel_id,
                        }) = last_upload.clone()
                        {
                            let _ = ui_tx.try_send(UiEvent::Info("Retrying upload".to_string()));
                            let _ = ui_tx.try_send(UiEvent::UploadState(
                                AttachmentUploadUiState::InProgress {
                                    file_name: display_name.clone(),
                                    progress: 0.0,
                                },
                            ));
                            if let Some(task) = upload_task.take() {
                                task.abort();
                            }
                            let ui_tx_clone = ui_tx.clone();
                            let client_clone = client.clone();
                            upload_task = Some(tokio::spawn(async move {
                                match tokio::fs::read(&file_path).await {
                                    Ok(file_bytes) => match client_clone
                                        .upload_file(
                                            guild_id,
                                            channel_id,
                                            mime_type.as_deref(),
                                            file_bytes,
                                        )
                                        .await
                                    {
                                        Ok(file_id) => {
                                            let _ =
                                                ui_tx_clone.try_send(UiEvent::AttachmentUploaded {
                                                    upload_id,
                                                    file_id,
                                                    file_name: display_name.clone(),
                                                });
                                            let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Completed {
                                                    file_name: display_name,
                                                },
                                            ));
                                        }
                                        Err(err) => {
                                            let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                                AttachmentUploadUiState::Failed {
                                                    file_name: display_name,
                                                    error: err.to_string(),
                                                },
                                            ));
                                        }
                                    },
                                    Err(err) => {
                                        let _ = ui_tx_clone.try_send(UiEvent::UploadState(
                                            AttachmentUploadUiState::Failed {
                                                file_name: display_name,
                                                error: format!("failed reading local file: {err}"),
                                            },
                                        ));
                                    }
                                }
                            }));
                        }
                    }
                    BackendCommand::CancelUpload => {
                        if let Some(task) = upload_task.take() {
                            task.abort();
                        }
                        let _ = ui_tx.try_send(UiEvent::UploadState(AttachmentUploadUiState::Idle));
                        let _ = ui_tx.try_send(UiEvent::Info("Upload canceled".to_string()));
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

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Prototype RTC Desktop GUI",
        options,
        Box::new(|_cc| Ok(Box::new(DesktopGuiApp::new(cmd_tx, ui_rx)))),
    )
}
