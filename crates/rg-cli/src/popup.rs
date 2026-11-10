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

fn font_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string()))
        .join("Fonts")
}

fn read_font(names: &[&str]) -> Option<egui::FontData> {
    let dir = font_dir();
    for name in names {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            return Some(egui::FontData::from_owned(bytes));
        }
    }
    None
}

/// Segoe UI for Latin + Nirmala UI (Windows Indic) for Bengali, mirroring
/// the main GUI's theme fallback. Without the Nirmala fallback, Bangla
/// titles render as tofu boxes in this standalone window.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let regular = read_font(&["SegUIVar.ttf", "segoeui.ttf"]);
    if let Some(data) = regular {
        fonts
            .font_data
            .insert("rg-ui".to_string(), std::sync::Arc::new(data));
        if let Some(chain) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            chain.insert(0, "rg-ui".to_string());
        }
    }
    let bangla = read_font(&["Nirmala.ttf", "NotoSansBengali-Regular.ttf"]);
    if let Some(data) = bangla {
        fonts
            .font_data
            .insert("rg-bangla".to_string(), std::sync::Arc::new(data));
        if let Some(chain) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            let at = if chain.first().is_some_and(|f| f == "rg-ui") {
                1
            } else {
                0
            };
            chain.insert(at, "rg-bangla".to_string());
        }
    }
    if let Some(data) = read_font(&["NirmalaB.ttf", "NotoSansBengali-Bold.ttf"]) {
        fonts
            .font_data
            .insert("rg-bangla-bold".to_string(), std::sync::Arc::new(data));
        if let Some(chain) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            chain.push("rg-bangla-bold".to_string());
        }
    }
    ctx.set_fonts(fonts);
}

// Mirrors crates/rg-gui/src/ui/theme.rs (standalone crate can't import it).
const BG: egui::Color32 = egui::Color32::from_rgb(0x20, 0x20, 0x20);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x2B, 0x2B, 0x2B);
const CONTROL: egui::Color32 = egui::Color32::from_rgb(0x37, 0x37, 0x37);
const CONTROL_HOVER: egui::Color32 = egui::Color32::from_rgb(0x40, 0x40, 0x40);
const CONTROL_PRESSED: egui::Color32 = egui::Color32::from_rgb(0x32, 0x32, 0x32);
const STROKE: egui::Color32 = egui::Color32::from_rgb(0x33, 0x33, 0x33);
const STROKE_SOFT: egui::Color32 = egui::Color32::from_rgb(0x2A, 0x2A, 0x2A);
const TEXT: egui::Color32 = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
const TEXT_SECONDARY: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0xC5, 0xC5, 0xC5, 0xC5);
const TEXT_TERTIARY: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(0x8B, 0x8B, 0x8B, 0x8B);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x60, 0xCD, 0xFF);
const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0x78, 0xD4, 0xFF);
const ACCENT_PRESSED: egui::Color32 = egui::Color32::from_rgb(0x4C, 0xB4, 0xE6);
const ON_ACCENT: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0xE6);
const WARNING: egui::Color32 = egui::Color32::from_rgb(0xFC, 0xE1, 0x00);

const SP_S: f32 = 8.0;
const SP_M: f32 = 12.0;
const R_MEDIUM: f32 = 8.0;
const BTN_H: f32 = 36.0;
const ANIM: f32 = 0.14;

#[derive(Default, Clone)]
struct ProbeState {
    size: Option<u64>,
    content_type: Option<String>,
    probing: bool,
    /// True when `size` may be a single 206 range chunk rather than the
    /// whole file (HEAD/range probe never reported a total).
    size_partial: bool,
}

struct PopupApp {
    capture: CaptureRequest,
    display_name: String,
    /// Directory the file saves into (editable via Browse).
    folder: String,
    /// Editable file-name stem (no extension).
    name_stem: String,
    /// Locked extension including the dot (e.g. ".mp4"), empty if none.
    name_ext: String,
    dont_ask_again: bool,
    copied_at: Option<std::time::Instant>,
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

/// Default Save-as name and path, routed into the per-category folder when
/// the shared config enables organizing (the GUI writes `settings.json` on
/// save). An existing file becomes `name (1).ext` so nothing is overwritten.
fn suggested_output_routed(filename: &str, content_type: Option<&str>) -> (String, String) {
    let config = rg_config::AppConfig::load();
    let dir = config.dir_for(&download_dir(), filename, content_type);
    let name = rg_core::unique_file_name(&dir, filename);
    let path = dir.join(&name).display().to_string();
    (name, path)
}

/// Pretty display host for header chips: prefer the page (referer) host —
/// e.g. `facebook.com` — over the sniffed CDN host, which is a long
/// `scontent-…fbcdn.net` string.
fn pretty_host(url: &str) -> String {
    let s = url.trim();
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split(['/']).next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    host.trim_start_matches("www.").to_ascii_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Video,
    Audio,
    Image,
    File,
}

impl FileKind {
    fn from_parts(filename: &str, content_type: Option<&str>) -> Self {
        let ct = content_type.unwrap_or("").to_ascii_lowercase();
        if ct.starts_with("video/") || ct.starts_with("audio/") || ct.starts_with("image/") {
            return match ct.split('/').next().unwrap_or("") {
                "video" => FileKind::Video,
                "audio" => FileKind::Audio,
                _ => FileKind::Image,
            };
        }
        let ext = filename
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi" | "flv" => FileKind::Video,
            "mp3" | "m4a" | "wav" | "ogg" | "flac" => FileKind::Audio,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" => FileKind::Image,
            _ => FileKind::File,
        }
    }

}

/// Split a full name into its editable stem and locked extension.
fn split_name(name: &str) -> (String, String) {
    if let Some(dot) = name.rfind('.') {
        if dot > 0 && name.len() < dot + 8 {
            return (name[..dot].to_string(), name[dot..].to_string());
        }
    }
    (name.to_string(), String::new())
}

/// Strip characters that Windows forbids in file names; used for the
/// editable stem so a pasted path can't escape the save folder.
fn sanitize_stem(stem: &str) -> String {
    rg_core::sanitize_file_stem(stem)
}

/// Keep the head and tail visible for long paths
/// (`C:\Users\…\Downloads\Video`), ellipsizing the middle.
fn ellipsize_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head = max / 2;
    let tail = max - head - 1;
    let head: String = chars.iter().take(head).collect();
    let tail: String = chars.iter().rev().take(tail).collect::<String>().chars().rev().collect();
    format!("{head}…{tail}")
}

/// Ellipsize a URL at the end (keeps the beginning visible).
fn ellipsize_end(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head: String = chars.iter().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn text_width(ui: &egui::Ui, text: &str, font: egui::FontId) -> f32 {
    ui.ctx()
        .fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, TEXT))
        .size()
        .x
}



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BtnKind {
    Primary,
    Secondary,
    Ghost,
}

fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgba_premultiplied(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()), f(a.a(), b.a()))
}

fn transparent() -> egui::Color32 {
    egui::Color32::from_rgba_premultiplied(0, 0, 0, 0)
}

/// Fixed-width button mirroring `rg-gui`'s `widgets::button` (Primary /
/// Secondary / Ghost, animated hover, 8px radius).
fn action_button(ui: &mut egui::Ui, label: &str, kind: BtnKind, width: f32, height: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let t = ui.ctx().animate_bool_with_time(response.id, hovered, ANIM);
    let rounding = egui::CornerRadius::same(R_MEDIUM as u8);
    let (fill, text_color, stroke) = match kind {
        BtnKind::Primary => {
            let fill = if pressed {
                ACCENT_PRESSED
            } else {
                mix(ACCENT, ACCENT_HOVER, t)
            };
            (fill, ON_ACCENT, egui::Stroke::NONE)
        }
        BtnKind::Secondary => {
            let fill = if pressed {
                CONTROL_PRESSED
            } else {
                mix(CONTROL, CONTROL_HOVER, t)
            };
            (fill, TEXT, egui::Stroke::new(1.0, STROKE))
        }
        BtnKind::Ghost => {
            let fill = if pressed {
                CONTROL_PRESSED
            } else {
                mix(transparent(), CONTROL_HOVER, t)
            };
            (fill, TEXT_SECONDARY, egui::Stroke::NONE)
        }
    };
    let painter = ui.painter();
    if fill.a() > 0 {
        painter.rect_filled(rect, rounding, fill);
    }
    if stroke != egui::Stroke::NONE {
        painter.rect_stroke(rect, rounding, stroke, egui::StrokeKind::Middle);
    }
    if response.has_focus() {
        let ring = if kind == BtnKind::Primary { TEXT } else { ACCENT };
        painter.rect_stroke(
            rect.expand(2.0),
            rounding,
            egui::Stroke::new(1.5, ring),
            egui::StrokeKind::Middle,
        );
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(14.0),
        text_color,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 36×36 painted file-type badge (no font dependency, crisp at any DPI):
/// video = play triangle, audio = three bars, image = mountains + sun,
/// file = page with a folded corner. Tinted accent on CONTROL.
fn file_badge(ui: &mut egui::Ui, kind: FileKind) {
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::hover());
    let painter = ui.painter();
    let r = egui::CornerRadius::same(10);
    painter.rect_filled(rect, r, CONTROL);
    painter.rect_stroke(rect, r, egui::Stroke::new(1.0, STROKE_SOFT), egui::StrokeKind::Middle);
    let cx = rect.center().x;
    let cy = rect.center().y;
    match kind {
        FileKind::Video => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(cx - 6.0, cy - 8.0),
                    egui::pos2(cx + 8.0, cy),
                    egui::pos2(cx - 6.0, cy + 8.0),
                ],
                ACCENT,
                egui::Stroke::NONE,
            ));
        }
        FileKind::Audio => {
            for (i, &h) in [6.0_f32, 12.0, 8.0].iter().enumerate() {
                let x = cx - 9.0 + i as f32 * 7.0;
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        egui::pos2(x, cy),
                        egui::vec2(3.0, h),
                    ),
                    egui::CornerRadius::same(1),
                    ACCENT,
                );
            }
        }
        FileKind::Image => {
            painter.circle_filled(egui::pos2(cx - 4.0, cy - 5.0), 3.0, ACCENT);
            painter.add(egui::Shape::line_segment(
                [egui::pos2(cx - 9.0, cy + 8.0), egui::pos2(cx - 2.0, cy - 1.0)],
                egui::Stroke::new(2.5, ACCENT),
            ));
            painter.add(egui::Shape::line_segment(
                [egui::pos2(cx + 0.0, cy + 3.0), egui::pos2(cx + 9.0, cy + 8.0)],
                egui::Stroke::new(2.5, ACCENT),
            ));
        }
        FileKind::File => {
            let doc = egui::Rect::from_center_size(egui::pos2(cx + 1.0, cy), egui::vec2(14.0, 18.0));
            painter.rect_filled(doc, egui::CornerRadius::same(2), ACCENT);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(doc.right() - 5.0, doc.top()),
                    egui::pos2(doc.right(), doc.top() + 5.0),
                    egui::pos2(doc.right() - 5.0, doc.top() + 5.0),
                ],
                mix(ACCENT, TEXT, 0.35),
                egui::Stroke::NONE,
            ));
        }
    }
}

/// Subtle-background field: fixed-size CONTROL box that never overflows
/// its allocated width (painted rect + truncating label inside).
fn field_box(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    text: &str,
    color: egui::Color32,
    tooltip: &str,
) -> egui::Response {
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(6), CONTROL);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(6),
        egui::Stroke::new(1.0, STROKE_SOFT),
        egui::StrokeKind::Middle,
    );
    // Left-aligned text, vertically centered in the box.
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 10.0, rect.top() + 2.0),
        egui::pos2(rect.right() - 10.0, rect.bottom() - 2.0),
    );
    let resp = ui.put(
        text_rect,
        egui::Label::new(egui::RichText::new(text).color(color).size(12.0))
            
            .truncate(),
    );
    resp.on_hover_text(tooltip.to_string())
}

/// Small metadata pill: tinted fill + colored 12px label.
fn chip(ui: &mut egui::Ui, text: &str, color: egui::Color32, tint: f32) -> egui::Response {
    let font = egui::FontId::proportional(12.0);
    let w = text_width(ui, text, font.clone()) + 16.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(10), mix(transparent(), color, tint));
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, font, color);
    resp
}

/// High-contrast checkbox: 20px box, 1px border, accent fill + dark check
/// when on. The whole row is clickable; label is dimmed until hovered.
fn checkbox_row(ui: &mut egui::Ui, checked: &mut bool, label: &str) {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 24.0),
        egui::Sense::click(),
    );
    let hovered = resp.hovered();
    let box_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::vec2(20.0, 20.0),
    );
    let painter = ui.painter();
    if *checked {
        painter.rect_filled(box_rect, egui::CornerRadius::same(4), ACCENT);
        let p = |x: f32, y: f32| {
            egui::pos2(box_rect.left() + box_rect.width() * x, box_rect.top() + box_rect.height() * y)
        };
        let stroke = egui::Stroke::new(2.2, ON_ACCENT);
        painter.line_segment([p(0.22, 0.52), p(0.42, 0.71)], stroke);
        painter.line_segment([p(0.42, 0.71), p(0.79, 0.30)], stroke);
    } else {
        // 1px border with strong contrast on the dark card.
        painter.rect_filled(box_rect, egui::CornerRadius::same(4), CONTROL);
        painter.rect_stroke(
            box_rect,
            egui::CornerRadius::same(4),
            egui::Stroke::new(1.5, TEXT_SECONDARY),
            egui::StrokeKind::Middle,
        );
    }
    if resp.has_focus() {
        painter.rect_stroke(
            box_rect.expand(2.0),
            egui::CornerRadius::same(6),
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Middle,
        );
    }
    painter.text(
        egui::pos2(box_rect.right() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        if hovered || *checked { TEXT } else { TEXT_SECONDARY },
    );
    if resp.clicked() {
        *checked = !*checked;
        resp.request_focus();
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
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
    fn full_name(&self) -> String {
        format!("{}{}", sanitize_stem(&self.name_stem), self.name_ext)
    }

    fn output_path(&self) -> String {
        std::path::Path::new(&self.folder)
            .join(self.full_name())
            .display()
            .to_string()
    }

    fn confirm(&self) -> CaptureRequest {
        CaptureRequest {
            url: self.capture.url.clone(),
            filename: self.capture.filename.clone(),
            referer: self.capture.referer.clone(),
            cookies: self.capture.cookies.clone(),
            user_agent: self.capture.user_agent.clone(),
            title: self.capture.title.clone(),
            content_disposition_filename: self.capture.content_disposition_filename.clone(),
            content_type: self.capture.content_type.clone(),
            content_length: self.capture.content_length.clone(),
            source: self.capture.source.clone(),
            auto_start: false,
            output: Some(self.output_path()),
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
        // A "media" capture is a page only when its URL carries no media file
        // name. MSE players (Facebook) resolve to the sniffed CDN stream URL,
        // which is a direct download just like any other file.
        let is_media_page = self.capture.source.as_deref() == Some("media")
            && !rg_bridge::is_media_file_url(&self.capture.url);
        let (size_label, type_label) = if is_media_page {
            // The resolver runs on Start; there is no probe type yet. Keep
            // this in the tooltip instead of a permanent "RESOLVED" chip.
            ("Video page".to_string(), String::new())
        } else {
            // While probing, the stored size may be the browser's partial
            // (206 range) value — don't show it until the probe settles.
            let size_label = if probe.probing {
                "Resolving…".to_string()
            } else {
                match probe.size {
                    Some(bytes) => human_bytes(bytes),
                    None => "Unknown size".to_string(),
                }
            };
            let type_label = probe
                .content_type
                .as_deref()
                .unwrap_or(if probe.probing { "…" } else { "—" })
                .to_string();
            (size_label, type_label)
        };
        // Preferred host for chips: referer page host wins over the CDN host.
        let host_label = self
            .capture
            .referer
            .as_deref()
            .map(|r| pretty_host(r))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| pretty_host(&self.capture.url));
        let kind = FileKind::from_parts(
            &self.display_name,
            probe.content_type.as_deref().or(self.capture.content_type.as_deref()),
        );
        // Short type chip: "MP4" not "video/mp4".
        let type_chip = type_label
            .rsplit('/')
            .next()
            .unwrap_or(&type_label)
            .to_ascii_uppercase();
        let display_host = host_label.clone();
        let full_name = self.full_name();
        let captured_url = self.capture.url.clone();
        let copied_active = self
            .copied_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
        if self.copied_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(1500)) {
            self.copied_at = None;
        }

        // Footer pinned to the window bottom, no divider — separation by
        // spacing only. Hints left, actions right.
        egui::Panel::bottom("rg-popup-footer")
            .frame(
                egui::Frame::default()
                    .fill(CARD)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui: &mut egui::Ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("Enter to start · Esc to cancel")
                                .color(TEXT_TERTIARY)
                                .size(11.0),
                        ),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = SP_S;
                        // Fixed widths sized so the hint + buttons fit the
                        // 488px inner width without clipping at any DPI.
                        if action_button(ui, "Start download", BtnKind::Primary, 124.0, BTN_H).clicked() {
                            let confirm = self.confirm();
                            self.finish(ui.ctx(), Some(confirm));
                        }
                        if action_button(ui, "Download Later", BtnKind::Secondary, 112.0, BTN_H).clicked() {
                            let confirm = self.confirm();
                            self.finish(ui.ctx(), Some(confirm));
                        }
                        if action_button(ui, "Cancel", BtnKind::Ghost, 64.0, BTN_H).clicked() {
                            self.finish(ui.ctx(), None);
                        }
                    });
                });
            });

        // Card surface: slightly lighter than the window background via
        // CARD fill with a 12px radius, everything on one left edge.
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(CARD)
                    .stroke(egui::Stroke::new(1.0, STROKE_SOFT))
                    .corner_radius(egui::CornerRadius::same(12))
                    // Single 16px left edge, 8px top/bottom.
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui: &mut egui::Ui| {
                ui.spacing_mut().item_spacing.y = SP_S;

                // --- Header: badge + editable name + chips -----------------
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SP_M;
                    file_badge(ui, kind);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        // Inline-editable name stem; the dimmed extension
                        // sits DIRECTLY after the field on the same line
                        // (no gap, baseline-aligned) and cannot be edited.
                        let name_font = egui::FontId::proportional(16.0);
                        let ext_w = if self.name_ext.is_empty() {
                            0.0
                        } else {
                            text_width(ui, &self.name_ext, egui::FontId::proportional(12.0))
                        };
                        let stem_w = text_width(ui, &self.name_stem, name_font.clone());
                        // Auto-size to content up to the max width; past
                        // that the name ellipsizes (label) / scrolls (edit).
                        let max_w = (ui.available_width() - ext_w).max(80.0);
                        let field_w = (stem_w + 16.0).clamp(80.0, max_w);
                        // Field + extension on ONE line (a horizontal row
                        // inside the vertical stack); ZERO gap between the
                        // field and the baseline-aligned dim extension.
                        // At max width the stem ellipsizes via the "…"
                        // overlay while still editing in place.
                        let ext_text = self.name_ext.clone();
                        let full_name2 = full_name.clone();
                        let overflow = stem_w + 16.0 > field_w;
                        let resp = ui
                            .horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                let edit = egui::TextEdit::singleline(&mut self.name_stem)
                                    .id(ui.make_persistent_id("rg-popup-name"))
                                    .frame(egui::Frame::NONE)
                                    .margin(egui::Margin::ZERO)
                                    .font(name_font.clone())
                                    .text_color(TEXT);
                                let resp = ui.add_sized(egui::vec2(field_w, 28.0), edit);
                                if !resp.has_focus() {
                                    resp.clone().on_hover_text(full_name2.clone());
                                }
                                if !ext_text.is_empty() {
                                    ui.add_sized(
                                        egui::vec2(ext_w.max(1.0), 28.0),
                                        egui::Label::new(
                                            egui::RichText::new(&ext_text)
                                                .color(TEXT_TERTIARY)
                                                .size(12.0),
                                        ),
                                    );
                                }
                                resp
                            })
                            .inner;
                        if overflow && !resp.has_focus() {
                            ui.painter().text(
                                egui::pos2(resp.rect.right() - 8.0, resp.rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                "…",
                                name_font.clone(),
                                TEXT,
                            );
                        }
                        // Underline on hover/focus.
                        let underline = if resp.has_focus() {
                            Some(ACCENT)
                        } else if resp.hovered() {
                            Some(STROKE)
                        } else {
                            None
                        };
                        if let Some(color) = underline {
                            let y = resp.rect.bottom() - 1.0;
                            ui.painter().line_segment(
                                [
                                    egui::pos2(resp.rect.left() + 1.0, y),
                                    egui::pos2(resp.rect.right() - 1.0, y),
                                ],
                                egui::Stroke::new(2.0, color),
                            );
                        }
                        // Chips: size · type · host, plus a warning when the
                        // probed size may be a partial range chunk. Wrapped
                        // so a long host never overflows the card.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let size = chip(ui, &size_label, TEXT_SECONDARY, 0.14);
                            if is_media_page {
                                size.on_hover_text(
                                    "The media URL is resolved when the download starts.",
                                );
                            }
                            if !type_chip.is_empty() && type_chip != "—" && type_chip != "…" {
                                chip(ui, &type_chip, TEXT_SECONDARY, 0.14);
                            }
                            if !display_host.is_empty() {
                                chip(ui, &display_host, TEXT_SECONDARY, 0.14);
                            }
                            if probe.size_partial && !probe.probing {
                                chip(ui, "Size may be incomplete", WARNING, 0.14).on_hover_text(
                                    "The server didn't report a full size; the final size may differ.",
                                );
                            }
                        });
                    });
                });

                // --- Save to: label above field (+ Change button) ------------
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Save to")
                            .color(TEXT_SECONDARY)
                            .size(12.0),
                    ),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SP_S;
                    // Field + Change exactly fill the row — nothing can
                    // overflow or clip at any DPI.
                    let row_w = ui.available_rect_before_wrap().width();
                    let field_w = (row_w - 76.0 - SP_S).max(80.0);
                    let display_folder = ellipsize_middle(&self.folder, 52);
                    field_box(
                        ui,
                        field_w,
                        32.0,
                        &display_folder,
                        TEXT_SECONDARY,
                        &self.folder.clone(),
                    );
                    if action_button(ui, "Change", BtnKind::Secondary, 76.0, 32.0).clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.folder = dir.display().to_string();
                        }
                    }
                });

                ui.add_space(SP_S);

                // --- Source: label above field (+ copy icon button) ----------
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Source")
                            .color(TEXT_SECONDARY)
                            .size(12.0),
                    ),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SP_S;
                    // SAME geometry as Save to: 76px tonal button + field
                    // filling the rest, so both left and right edges line
                    // up exactly.
                    let row_w = ui.available_rect_before_wrap().width();
                    let field_w = (row_w - 76.0 - SP_S).max(80.0);
                    let display_url = ellipsize_end(&captured_url, 64);
                    field_box(ui, field_w, 32.0, &display_url, TEXT_TERTIARY, &captured_url.clone());
                    let copy_label = if copied_active { "Copied" } else { "Copy" };
                    if action_button(ui, copy_label, BtnKind::Secondary, 76.0, 32.0).clicked() {
                        ui.ctx().copy_text(captured_url.clone());
                        self.copied_at = Some(std::time::Instant::now());
                    }
                });

                checkbox_row(ui, &mut self.dont_ask_again, "Don't ask again for this site");
            });

        // Keyboard: Enter starts, Escape cancels.
        if ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
            let confirm = self.confirm();
            self.finish(ui.ctx(), Some(confirm));
        } else if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            self.finish(ui.ctx(), None);
        }

        if self.result.lock().unwrap().is_none() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// Total size from a `Content-Range: bytes 0-0/123456` header (the part
/// after '/'), which stays correct on `206` responses where
/// `Content-Length` only covers the requested range.
fn range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let s = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())?;
    s.rfind('/')
        .and_then(|slash| s[slash + 1..].trim().parse::<u64>().ok())
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn spawn_probe(capture: CaptureRequest, probe: Arc<Mutex<ProbeState>>) {
    // Always probe: the browser-reported Content-Length can be a single
    // range chunk (e.g. a Facebook CDN 206), which would show a bogus
    // small size in the popup. The capture value stays as a fallback and is
    // only replaced when the probe returns a real size.
    probe.lock().unwrap().probing = true;
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(_) => {
                probe.lock().unwrap().probing = false;
                return;
            }
        };
        let authed = |mut req: reqwest::blocking::RequestBuilder| {
            if let Some(ua) = &capture.user_agent {
                req = req.header(reqwest::header::USER_AGENT, ua.as_str());
            }
            if let Some(cookie) = &capture.cookies {
                req = req.header(reqwest::header::COOKIE, cookie.as_str());
            }
            if let Some(referer) = &capture.referer {
                req = req.header(reqwest::header::REFERER, referer.as_str());
            }
            req
        };

        let mut size: Option<u64> = None;
        let mut ctype: Option<String> = None;
        let mut got_total = false;

        // 1) HEAD: prefer Content-Range total over Content-Length.
        if let Ok(resp) = authed(client.head(&capture.url)).send() {
            if resp.status().is_success() {
                let headers = resp.headers().clone();
                if range_total(&headers).is_some() {
                    got_total = true;
                }
                size = range_total(&headers)
                    .or_else(|| header_u64(&headers, reqwest::header::CONTENT_LENGTH));
                ctype = headers
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
            }
        }

        // 2) One-byte ranged GET: HEAD alone can still report a chunk size
        // on range-capable CDNs. Headers are read without consuming the
        // body, so this never downloads the file. The 206 Content-Range
        // total is authoritative when present.
        {
            if let Ok(resp) = authed(
                client
                    .get(&capture.url)
                    .header(reqwest::header::RANGE, "bytes=0-0"),
            )
            .send()
            {
                if resp.status().is_success()
                    || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
                {
                    let headers = resp.headers().clone();
                    if let Some(total) = range_total(&headers) {
                        size = Some(total);
                        got_total = true;
                    } else if size.is_none() {
                        size = header_u64(&headers, reqwest::header::CONTENT_LENGTH);
                    }
                    if ctype.is_none() {
                        ctype = headers
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                    }
                }
            }
        }

        let mut state = probe.lock().unwrap();
        // Only replace the fallback size with a probed one; keep the
        // browser-reported Content-Type when the probe has none. If the
        // probe never reported a total and the file is a video or
        // suspiciously small, the number may be one 206 range chunk.
        if size.is_some() {
            state.size = size;
        }
        if state.content_type.is_none() {
            state.content_type = ctype;
        }
        let kind = FileKind::from_parts(
            &capture.url,
            state.content_type.as_deref(),
        );
        state.size_partial = !got_total
            && state
                .size
                .is_some_and(|n| kind == FileKind::Video || n < 256 * 1024);
        state.probing = false;
    });
}

/// Default Save-as name for a capture.
///
/// All title cleaning lives in rg-core so the popup and the GUI agree:
/// Content-Disposition (unless generic) → cleaned page/video title → browser
/// filename → readable URL basename → dated site fallback.
fn default_display_name(capture: &CaptureRequest) -> String {
    rg_core::default_capture_name(&rg_core::NamingContext {
        title: capture.title.as_deref(),
        content_disposition: capture.content_disposition_filename.as_deref(),
        filename: capture.filename.as_deref(),
        url: &capture.url,
        referer: capture.referer.as_deref(),
        content_type: capture.content_type.as_deref(),
        media_capture: capture.source.as_deref() == Some("media"),
    })
}

fn main() -> Result<()> {
    let capture = rg_bridge::read_frame(std::io::stdin())?;

    let default_name = default_display_name(&capture);
    let (display_name, output) = match capture.output.clone() {
        Some(output) => (default_name, output),
        None => suggested_output_routed(&default_name, capture.content_type.as_deref()),
    };
    let (name_stem, name_ext) = split_name(&display_name);
    let folder = std::path::Path::new(&output)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| output.clone());

    let probe = Arc::new(Mutex::new(ProbeState {
        size: capture.content_length,
        content_type: capture.content_type.clone(),
        probing: false,
        size_partial: false,
    }));
    spawn_probe(capture.clone(), probe.clone());

    let result: Arc<Mutex<Option<Option<CaptureRequest>>>> = Arc::new(Mutex::new(None));
    let app = PopupApp {
        capture,
        display_name,
        folder,
        name_stem,
        name_ext,
        dont_ask_again: false,
        copied_at: None,
        probe,
        result: result.clone(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("New download — RapidGrab")
            .with_inner_size([520.0, 304.0])
            .with_resizable(false)
            .with_active(true),
        ..Default::default()
    };

    eframe::run_native(
        "New download — RapidGrab",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
            install_fonts(&cc.egui_ctx);
            let mut visuals = egui::Visuals::dark();
            visuals.dark_mode = true;
            visuals.panel_fill = BG;
            visuals.window_fill = CARD;
            visuals.window_stroke = egui::Stroke::new(1.0, STROKE_SOFT);
            visuals.window_corner_radius = egui::CornerRadius::same(12);
            visuals.button_frame = true;
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
                title: None,
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
