#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod args;
mod autostart;
mod icon;
mod tray;
mod ui;
mod virus;

use eframe::egui;
use tracing_subscriber::EnvFilter;

fn main() -> eframe::Result<()> {
    let start_minimized = std::env::args().any(|a| a == "--minimized");
    // Daily rolling file log under %APPDATA%\RapidGrab\logs\ (Phase 4), in
    // addition to the console logger in debug builds.
    let log_dir = rg_config::logs_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = tracing_appender::rolling::daily(log_dir, "rapidgrab.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rg_core=info,rg_gui=info")),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([860.0, 720.0])
        .with_min_inner_size([640.0, 520.0])
        .with_title("RapidGrab")
        .with_icon(icon::window_icon())
        .with_visible(!start_minimized);

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "RapidGrab",
        native_options,
        Box::new(|cc| Ok(Box::new(app::RapidGrabApp::new(cc)))),
    )
}
