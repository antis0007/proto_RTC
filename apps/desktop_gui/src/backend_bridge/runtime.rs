//! Runtime bridge between UI command queue and backend event intake.

use crossbeam_channel::{Receiver, Sender};

use crate::backend_bridge::commands::BackendCommand;
use crate::controller::events::UiEvent;

pub fn launch(cmd_rx: Receiver<BackendCommand>, ui_tx: Sender<UiEvent>) {
    crate::ui::app::start_backend_bridge(cmd_rx, ui_tx);
}
