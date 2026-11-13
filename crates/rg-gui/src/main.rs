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
            // `rg_resolve` carries the stream-pairing and mux decisions; without
            // it a silent (video-only) download leaves no trace in the log.
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // The GUI binary target is `rapidgrab-gui`, so its tracing
                // target is `rapidgrab_gui` (not the package name `rg_gui`).
                EnvFilter::new("rg_core=info,rg_resolve=info,rapidgrab_gui=info,rg_gui=info")
            }),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    // Which binary is actually running: a stale build (for example an old
    // `target\debug` copy) silently ignores capture fields it does not know,
    // so the version and path belong in every log.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = if cfg!(debug_assertions) { "debug" } else { "release" },
        exe = %std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        "RapidGrab starting"
    );

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
