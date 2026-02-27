//! Desktop GUI bootstrap and entrypoint.
//!
//! # Architecture Rule
//! `main.rs` must only contain CLI parsing/bootstrap, dependency wiring, and
//! `eframe::run_native` startup. Feature logic belongs in `ui/`, `controller/`,
//! `backend_bridge/`, or `media/` modules.

mod backend_bridge;
mod controller;
mod media;
mod ui;

use std::path::PathBuf;

use clap::Parser;
use crossbeam_channel::bounded;
use eframe::egui;
use ui::{DesktopGuiApp, StartupConfig};

#[derive(Parser, Debug)]
#[command(name = "desktop_gui")]
struct CliArgs {
    #[arg(long, default_value = "http://127.0.0.1:8443")]
    server_url: String,

    #[arg(long, default_value = "alice")]
    username: String,

    #[arg(long)]
    display_name: Option<String>,

    #[arg(long)]
    data_dir: Option<PathBuf>,
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();

    let args = CliArgs::parse();
    let startup = StartupConfig {
        server_url: args.server_url,
        username: args.username.clone(),
        display_name: args.display_name.unwrap_or_else(|| args.username.clone()),
        data_dir: args.data_dir,
    };

    let (cmd_tx, cmd_rx) = bounded(512);
    let (ui_tx, ui_rx) = bounded(512);
    backend_bridge::runtime::launch(cmd_rx, ui_tx);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1366.0, 768.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ProtoRTC Desktop GUI",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(DesktopGuiApp::bootstrap(
                cmd_tx.clone(),
                ui_rx.clone(),
                startup.clone(),
            )))
        }),
    )
}
