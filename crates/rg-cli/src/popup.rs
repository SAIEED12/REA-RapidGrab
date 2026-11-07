//! Standalone Download popup for RapidGrab (IDM "Download File Info" style).
//!
//! Spawned by `rapidgrab-host` per capture: reads one `CaptureRequest`
//! frame from stdin, shows its own small window (so the popup appears even
//! when the main GUI window is minimized or behind the browser), and writes
//! one result frame to stdout:
//! * Start / Download Later → confirmed capture with the chosen Save-as path
//! * Cancel / window close  → empty url (host then acks `{ok:false}` so the
//!   browser download is left alone)

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use rg_bridge::CaptureRequest;

const BG: egui::Color32 = egui::Color32::from_rgb(0x20, 0x20, 0x20);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x2B, 0x2B, 0x2B);
const TEXT: egui::Color32 = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
const TEXT_SECONDARY: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0xC5, 0xC5, 0xC5, 0xC5);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x60, 0xCD, 0xFF);
const ON_ACCENT: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0xE6);

#[derive(Default, Clone)]
struct ProbeState {
    size: Option<u64>,
    content_type: Option<String>,
    probing: bool,
}

struct PopupApp {
    capture: CaptureRequest,
    display_name: String,
    output: String,
    dont_ask_again: bool,
    probe: Arc<Mutex<ProbeState>>,
    result: Arc<Mutex<Option<Option<CaptureRequest>>>>,
}

fn download_dir() -> std::path::PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        std::path::PathBuf::from(profile).join("Downloads")
    } else {
        std::path::PathBuf::from(".")
    }
}

fn suggested_output(filename: &str) -> String {
    download_dir().join(filename).display().to_string()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

impl PopupApp {
    fn confirm(&self) -> CaptureRequest {
        CaptureRequest {
            url: self.capture.url.clone(),
            filename: self.capture.filename.clone(),
            referer: self.capture.referer.clone(),
            cookies: self.capture.cookies.clone(),
            user_agent: self.capture.user_agent.clone(),
            content_disposition_filename: self.capture.content_disposition_filename.clone(),
            content_type: self.capture.content_type.clone(),
            content_length: self.capture.content_length.clone(),
            source: self.capture.source.clone(),
            auto_start: false,
            output: Some(self.output.clone()),
            confirmed: true,
            dont_ask_again: self.dont_ask_again,
        }
    }

    fn finish(&self, ctx: &egui::Context, value: Option<CaptureRequest>) {
        *self.result.lock().unwrap() = Some(value);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for PopupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && self.result.lock().unwrap().is_none() {
            self.finish(&ctx, None);
        }
        let probe = self.probe.lock().unwrap().clone();
        let is_media_page = self.capture.source.as_deref() == Some("media");
        let (size_label, type_label) = if is_media_page {
            ("Video page".to_string(), "resolved on Start".to_string())
        } else {
            let size_label = match probe.size {
                Some(bytes) => human_bytes(bytes),
                None if probe.probing => "Resolving…".to_string(),
                None => "Unknown".to_string(),
            };
            let type_label = probe
                .content_type
                .as_deref()
                .unwrap_or(if probe.probing { "…" } else { "—" })
                .to_string();
            (size_label, type_label)
        };
        let source_label = self
            .capture
            .source
            .as_deref()
            .unwrap_or("browser")
            .to_string();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(CARD)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui: &mut egui::Ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(&self.display_name)
                        .strong()
                        .size(16.0)
                        .color(TEXT),
                ));
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{source_label} · {size_label} · {type_label}"))
                        .color(TEXT_SECONDARY)
                        .size(12.0),
                ));
                ui.add(egui::Label::new(
                    egui::RichText::new(&self.capture.url)
                        .color(TEXT_SECONDARY)
                        .size(12.0),
                ));
                ui.add(egui::Label::new(
                    egui::RichText::new("Save as").color(TEXT).size(13.0),
                ));
                ui.horizontal(|ui| {
                    let field_w = (ui.available_width() - 112.0).max(120.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.output)
                            .desired_width(field_w)
                            .margin(egui::Margin::same(8)),
                    );
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("Browse…").color(TEXT))
                                .min_size(egui::vec2(104.0, 32.0)),
                        )
                        .clicked()
                    {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            let name = std::path::Path::new(&self.display_name)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "download.bin".to_string());
                            self.output = dir.join(name).display().to_string();
                        }
                    }
                });

                ui.checkbox(&mut self.dont_ask_again, "Don't ask again for this site");

                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("Cancel").min_size(egui::vec2(88.0, 36.0)))
                        .clicked()
                    {
                        let confirm = self.confirm();
                        self.finish(ui.ctx(), None);
                        let _ = confirm;
                    }
                    if ui
                        .add(egui::Button::new("Download Later").min_size(egui::vec2(124.0, 36.0)))
                        .clicked()
                    {
                        let confirm = self.confirm();
                        self.finish(ui.ctx(), Some(confirm));
                    }
                    let start = ui.add(
                        egui::Button::new(egui::RichText::new("Start download").color(ON_ACCENT))
                            .fill(ACCENT)
                            .min_size(egui::vec2(132.0, 36.0)),
                    );
                    if start.clicked() {
                        let confirm = self.confirm();
                        self.finish(ui.ctx(), Some(confirm));
                    }
                });
            });

        if self.result.lock().unwrap().is_none() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

fn spawn_probe(capture: CaptureRequest, probe: Arc<Mutex<ProbeState>>) {
    let has_info = capture.content_length.is_some() || capture.content_type.is_some();
    if has_info {
        probe.lock().unwrap().probing = false;
        return;
    }
    probe.lock().unwrap().probing = true;
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        let mut req = client.head(&capture.url);
        if let Some(ua) = &capture.user_agent {
            req = req.header(reqwest::header::USER_AGENT, ua.as_str());
        }
        if let Some(cookie) = &capture.cookies {
            req = req.header(reqwest::header::COOKIE, cookie.as_str());
        }
        if let Some(referer) = &capture.referer {
            req = req.header(reqwest::header::REFERER, referer.as_str());
        }
        if let Ok(resp) = req.send() {
            if resp.status().is_success() {
                let size = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let ctype = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let mut state = probe.lock().unwrap();
                state.size = size;
                state.content_type = ctype;
            }
        }
        probe.lock().unwrap().probing = false;
    });
}

fn main() -> Result<()> {
    let capture = rg_bridge::read_frame(std::io::stdin())?;

    let display_name = capture
        .content_disposition_filename
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(capture.filename.as_deref().filter(|s| !s.trim().is_empty()))
        .unwrap_or(&capture.url)
        .to_string();
    let output = capture
        .output
        .clone()
        .unwrap_or_else(|| suggested_output(&display_name));

    let probe = Arc::new(Mutex::new(ProbeState {
        size: capture.content_length,
        content_type: capture.content_type.clone(),
        probing: false,
    }));
    spawn_probe(capture.clone(), probe.clone());

    let result: Arc<Mutex<Option<Option<CaptureRequest>>>> = Arc::new(Mutex::new(None));
    let app = PopupApp {
        capture,
        display_name,
        output,
        dont_ask_again: false,
        probe,
        result: result.clone(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Download")
            .with_inner_size([420.0, 360.0])
            .with_resizable(false)
            .with_active(true),
        ..Default::default()
    };

    eframe::run_native(
        "Download",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
            let mut visuals = egui::Visuals::dark();
            visuals.dark_mode = true;
            visuals.panel_fill = BG;
            visuals.window_fill = CARD;
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("popup window failed: {e:?}"))?;

    // If the window was closed without a result, treat it as Cancel.
    let final_result = result.lock().unwrap().clone().unwrap_or(None);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match final_result {
        // Cancel: empty-url frame the host turns into {ok:false}.
        Some(confirm) => rg_bridge::write_frame(&mut lock, &confirm),
        None => {
            let cancel = CaptureRequest {
                url: String::new(),
                filename: None,
                referer: None,
                cookies: None,
                user_agent: None,
                content_disposition_filename: None,
                content_type: None,
                content_length: None,
                source: None,
                auto_start: false,
                output: None,
                confirmed: false,
                dont_ask_again: false,
            };
            rg_bridge::write_frame(&mut lock, &cancel)
        }
    }?;
    lock.flush()?;
    Ok(())
}
