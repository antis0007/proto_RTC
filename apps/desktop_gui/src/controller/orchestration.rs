//! Command orchestration helpers from UI actions to backend command queue.

use crossbeam_channel::{Sender, TrySendError};

use crate::backend_bridge::commands::BackendCommand;

pub fn dispatch_backend_command(
    cmd_tx: &Sender<BackendCommand>,
    cmd: BackendCommand,
    status: &mut String,
) {
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

    match cmd_tx.try_send(cmd) {
        Ok(()) => tracing::debug!(command = cmd_name, "queued ui->backend command"),
        Err(TrySendError::Full(_)) => {
            *status = "UI command queue is full; please retry".to_string();
        }
        Err(TrySendError::Disconnected(_)) => {
            *status =
                "Backend command processor disconnected (possible startup/runtime failure); retry sign-in"
                    .to_string();
        }
    }
}
