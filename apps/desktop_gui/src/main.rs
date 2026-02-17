use std::{collections::HashMap, thread};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use client_core::{ClientEvent, ClientHandle, PassthroughCrypto, RealtimeClient};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use eframe::egui;
use shared::{
    domain::{ChannelId, ChannelKind, GuildId},
    protocol::{ChannelSummary, GuildSummary, MessagePayload, ServerEvent},
};

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
    SendMessage {
        text: String,
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
    Error(String),
    Server(ServerEvent),
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
    status: String,
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
            status: "Not logged in".to_string(),
            settings_open: false,
            theme: ThemeSettings::discord_default(),
            applied_theme: None,
        }
    }

    fn process_ui_events(&mut self) {
        while let Ok(event) = self.ui_rx.try_recv() {
            match event {
                UiEvent::LoginOk => {
                    self.status = "Logged in".to_string();
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
                UiEvent::Server(server_event) => match server_event {
                    ServerEvent::GuildUpdated { guild } => {
                        if !self.guilds.iter().any(|g| g.guild_id == guild.guild_id) {
                            self.guilds.push(guild);
                        }
                        if self.selected_guild.is_none() {
                            self.selected_guild = Some(guild.guild_id);
                            self.channels.clear();
                            queue_command(
                                &self.cmd_tx,
                                BackendCommand::ListChannels {
                                    guild_id: guild.guild_id,
                                },
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
                            self.channels.push(channel);
                        }
                    }
                    ServerEvent::MessageReceived { message } => {
                        self.messages
                            .entry(message.channel_id)
                            .or_default()
                            .push(message);
                    }
                    ServerEvent::Error(err) => {
                        self.status = format!("Server error: {}", err.message);
                    }
                    _ => {}
                },
            }
        }
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
                        self.channels.clear();
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
            ui.heading("Messages");
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
                            ui.label(format!("{}: {}", msg.sender_id.0, decoded));
                        }
                    } else {
                        ui.label("No messages yet");
                    }
                } else {
                    ui.label("Select a channel");
                }
            });

            ui.separator();
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.composer)
                    .hint_text("Type a message and press Enter"),
            );
            let should_send = response.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !self.composer.trim().is_empty();
            if should_send {
                let text = std::mem::take(&mut self.composer);
                queue_command(
                    &self.cmd_tx,
                    BackendCommand::SendMessage { text },
                    &mut self.status,
                );
                response.request_focus();
            }
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
