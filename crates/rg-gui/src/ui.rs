//! Modern minimal visual system for RapidGrab.
//!
//! A single accent (electric blue), neutral slate surfaces, generous radius
//! and spacing. Everything here is pure rendering — no application state.

use eframe::egui;

/// Surfaces.
pub const PANEL: egui::Color32 = egui::Color32::from_rgb(13, 17, 23); // #0D1117
pub const CARD: egui::Color32 = egui::Color32::from_rgb(22, 27, 34); // #161B22
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(48, 54, 61); // #30363D
pub const TRACK: egui::Color32 = egui::Color32::from_rgb(33, 38, 45); // #21262D
pub const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(139, 148, 158); // #8B949E

/// Electric blue — the one signature accent.
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(77, 163, 255); // #4DA3FF
pub const ACCENT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(59, 130, 246); // #3B82F6

pub const GREEN: egui::Color32 = egui::Color32::from_rgb(63, 185, 80);
pub const AMBER: egui::Color32 = egui::Color32::from_rgb(210, 153, 34);
pub const RED: egui::Color32 = egui::Color32::from_rgb(248, 81, 73);
pub const SLATE: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);

/// Apply the RapidGrab theme to a fresh context.
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.panel_fill = PANEL;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = egui::Color32::from_rgb(8, 12, 17);
    visuals.faint_bg_color = TRACK;
    visuals.widgets.noninteractive.bg_fill = TRACK;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.weak_bg_fill = TRACK;
    visuals.widgets.inactive.bg_fill = TRACK;
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(48, 54, 61);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 54, 61);
    visuals.widgets.active.weak_bg_fill = ACCENT_ACTIVE;
    visuals.widgets.active.bg_fill = ACCENT_ACTIVE;
    visuals.selection.bg_fill = ACCENT_ACTIVE;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
    visuals.hyperlink_color = ACCENT;
    visuals.code_bg_color = CARD;
    visuals.warn_fg_color = AMBER;
    visuals.error_fg_color = RED;
    ctx.set_visuals(visuals);

    ctx.set_fonts(egui::FontDefinitions::default());

    let mut style = egui::Style::default();
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(16.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.indent = 14.0;
    style.spacing.interact_size.y = 26.0;
    style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.active.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.open.rounding = egui::Rounding::same(8.0);
    ctx.set_style(style);
}

// --- typography ------------------------------------------------------------

pub fn title(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .size(20.0)
        .strong()
        .color(egui::Color32::WHITE)
}

pub fn heading(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(15.0).strong()
}

pub fn small(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(11.0).color(TEXT_DIM)
}

pub fn caption(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(10.0).color(TEXT_DIM)
}

// --- containers ------------------------------------------------------------

pub fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
        .rounding(egui::Rounding::same(12.0))
        .inner_margin(egui::Margin::same(16.0))
}

pub fn panel_frame() -> egui::Frame {
    egui::Frame::default().fill(PANEL)
}

/// Rounded pill with a tinted background, used for counters and status.
pub fn pill(ui: &mut egui::Ui, text: &str, color: egui::Color32, dot: bool) {
    egui::Frame::default()
        .fill(tint(color, 22))
        .stroke(egui::Stroke::new(1.0_f32, tint(color, 70)))
        .rounding(egui::Rounding::same(999.0))
        .inner_margin(egui::Margin {
            left: 8.0,
            right: 8.0,
            top: 3.0,
            bottom: 3.0,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                if dot {
                    ui.add(egui::Label::new(
                        egui::RichText::new("●").size(8.0).color(color),
                    ));
                }
                ui.add(egui::Label::new(
                    egui::RichText::new(text).size(11.0).color(color),
                ));
            });
        });
}

fn tint(color: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

// --- buttons ---------------------------------------------------------------

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text)
            .size(13.0)
            .strong()
            .color(egui::Color32::WHITE),
    )
    .fill(ACCENT_ACTIVE)
    .rounding(egui::Rounding::same(8.0))
    .stroke(egui::Stroke::NONE)
}

pub fn success_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text)
            .size(12.0)
            .strong()
            .color(egui::Color32::WHITE),
    )
    .fill(egui::Color32::from_rgb(35, 108, 49))
    .rounding(egui::Rounding::same(8.0))
    .stroke(egui::Stroke::NONE)
}

pub fn ghost_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(12.0).color(TEXT_DIM))
        .fill(egui::Color32::TRANSPARENT)
        .rounding(egui::Rounding::same(8.0))
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
}

pub fn text_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(12.0).color(ACCENT))
        .fill(egui::Color32::TRANSPARENT)
        .rounding(egui::Rounding::same(6.0))
        .stroke(egui::Stroke::NONE)
}

pub fn destructive_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(12.0).color(RED))
        .fill(egui::Color32::TRANSPARENT)
        .rounding(egui::Rounding::same(6.0))
        .stroke(egui::Stroke::NONE)
}

// --- feedback --------------------------------------------------------------

/// Inline alert bar (error / notice) tinted by `color`.
pub fn alert_bar(ui: &mut egui::Ui, text: &str, color: egui::Color32, icon: &str) {
    egui::Frame::default()
        .fill(tint(color, 16))
        .stroke(egui::Stroke::new(1.0_f32, tint(color, 55)))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin {
            left: 10.0,
            right: 10.0,
            top: 6.0,
            bottom: 6.0,
        })
        .show(ui, |ui| {
            ui.add(egui::Label::new(
                egui::RichText::new(format!("{icon}  {text}"))
                    .size(11.0)
                    .color(color),
            ));
        });
}

// --- domain helpers --------------------------------------------------------

/// Status → colour + label.
pub fn status_style(status: &crate::app::JobStatus) -> (egui::Color32, &'static str) {
    use crate::app::JobStatus;
    match status {
        JobStatus::Queued => (SLATE, "Queued"),
        JobStatus::Starting => (ACCENT, "Starting…"),
        JobStatus::Downloading => (ACCENT, "Downloading"),
        JobStatus::Resolving => (ACCENT, "Resolving…"),
        JobStatus::Merging => (ACCENT, "Merging…"),
        JobStatus::Pausing => (AMBER, "Pausing…"),
        JobStatus::Paused => (AMBER, "Paused"),
        JobStatus::Completed => (GREEN, "Completed"),
        JobStatus::Failed => (RED, "Failed"),
        JobStatus::Cancelled => (SLATE, "Cancelled"),
    }
}

pub fn status_pill(ui: &mut egui::Ui, status: &crate::app::JobStatus) {
    let (color, label) = status_style(status);
    pill(ui, label, color, true);
}

/// Rounded progress bar with a centred label, no default egui styling.
pub fn progress_bar(ui: &mut egui::Ui, fraction: f32, text: &str, accent: egui::Color32) {
    let frac = fraction.clamp(0.0, 1.0);
    let full = ui.available_width();
    let desired = egui::vec2(full, 16.0);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let radius = egui::Rounding::same(8.0);
    let painter = ui.painter().clone();

    painter.rect_filled(rect, radius, TRACK);
    if frac > 0.0 {
        let width = (rect.width() * frac).max(if frac > 0.0 { 6.0 } else { 0.0 });
        let fill_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.min.x + width, rect.max.y),
        );
        painter.rect_filled(fill_rect, radius, tint(accent, 190));
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(10.0),
        egui::Color32::WHITE,
    );
    response.on_hover_text(text);
}
