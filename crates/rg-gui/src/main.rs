#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod args;
mod icon;
mod ui;

use eframe::egui;
use tracing_subscriber::EnvFilter;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rg_core=info,rg_gui=info")),
        )
        .init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 720.0])
            .with_min_inner_size([640.0, 520.0])
            .with_title("RapidGrab")
            .with_icon(icon::window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "RapidGrab",
        native_options,
        Box::new(|cc| Ok(Box::new(app::RapidGrabApp::new(cc)))),
    )
}
