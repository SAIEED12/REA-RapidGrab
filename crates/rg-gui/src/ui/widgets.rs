//! Reusable Fluent-inspired widgets built on top of [`super::theme`].

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Id, Rect, Response, RichText, Rounding,
    Sense, Stroke, Ui, Vec2,
};

use super::theme::*;
use crate::app::JobStatus;

// --- helpers ---------------------------------------------------------------

/// Linear mix of two premultiplied colors.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

fn transparent() -> Color32 {
    Color32::from_rgba_premultiplied(0, 0, 0, 0)
}

fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
    ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, TEXT))
        .size()
        .x
}

// --- containers ------------------------------------------------------------

/// Standard card / layer surface.
pub fn card() -> egui::Frame {
    egui::Frame::default()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, STROKE_SOFT))
        .rounding(Rounding::same(R_CARD))
        .inner_margin(egui::Margin::same(SP_L))
        .shadow(egui::epaint::Shadow {
            offset: Vec2::new(0.0, 4.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_rgba_premultiplied(0, 0, 0, 0x38),
        })
}

/// 1px separator that spans the current layout width.
pub fn divider(ui: &mut Ui) {
    ui.add_space(SP_S);
    let x = ui.available_rect_before_wrap().x_range();
    let y = ui.cursor().top();
    ui.painter().hline(x, y, Stroke::new(1.0_f32, STROKE));
    ui.add_space(SP_S);
}

/// Lays out `add` in a column at most [`CONTENT_W`] wide, horizontally centered.
pub fn content_column<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let width = ui.available_width().min(CONTENT_W);
    let side = ((ui.available_width() - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui)
        })
        .inner
    })
    .inner
}

// --- buttons ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The single accent action on a screen.
    Primary,
    /// Outlined secondary action.
    Secondary,
    /// Borderless quiet action.
    Ghost,
}

/// Fluent button with animated hover/press states and an explicit disabled look.
pub fn button(ui: &mut Ui, label: &str, kind: ButtonKind, enabled: bool) -> Response {
    let font = FontId::new(BODY_SIZE, proportional());
    let pad_x = if kind == ButtonKind::Ghost { SP_S } else { SP_L };
    let min_w = if kind == ButtonKind::Primary { BUTTON_MIN_W } else { 0.0 };
    let size = Vec2::new(
        (text_width(ui, label, font.clone()) + pad_x * 2.0).max(min_w),
        if kind == ButtonKind::Ghost {
            CONTROL_H
        } else {
            BTN_H
        },
    );

    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, hovered, ANIM);

    let rounding = Rounding::same(R_MEDIUM);
    let (fill, text_color, stroke) = match kind {
        ButtonKind::Primary => {
            let fill = if !enabled {
                FIELD
            } else if pressed {
                ACCENT_PRESSED
            } else {
                mix(ACCENT, ACCENT_HOVER, t)
            };
            let text = if enabled { ON_ACCENT } else { TEXT_DISABLED };
            let stroke = if enabled {
                Stroke::NONE
            } else {
                Stroke::new(1.0_f32, STROKE_SOFT)
            };
            (fill, text, stroke)
        }
        ButtonKind::Secondary => {
            let fill = if !enabled {
                transparent()
            } else if pressed {
                CONTROL_PRESSED
            } else {
                mix(CONTROL, CONTROL_HOVER, t)
            };
            (
                fill,
                if enabled { TEXT } else { TEXT_DISABLED },
                Stroke::new(1.0_f32, STROKE),
            )
        }
        ButtonKind::Ghost => {
            let fill = if !enabled {
                transparent()
            } else {
                mix(transparent(), CONTROL_HOVER, t)
            };
            (
                fill,
                if enabled { TEXT_SECONDARY } else { TEXT_DISABLED },
                Stroke::NONE,
            )
        }
    };

    let painter = ui.painter();
    if fill.a() > 0 {
        painter.rect_filled(rect, rounding, fill);
    }
    if stroke != Stroke::NONE {
        painter.rect_stroke(rect, rounding, stroke);
    }
    if response.has_focus() {
        let ring = if kind == ButtonKind::Primary {
            TEXT
        } else {
            ACCENT
        };
        painter.rect_stroke(
            rect.expand(2.0),
            rounding,
            Stroke::new(1.5_f32, ring),
        );
    }
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, text_color);

    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// 28x28 borderless icon button with hover fill and tooltip.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(ICON_BTN), sense);
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, enabled && response.hovered(), ANIM);

    if t > 0.0 {
        ui.painter().rect_filled(
            rect,
            Rounding::same(R_SMALL),
            mix(transparent(), CONTROL_HOVER, t),
        );
    }
    let color = if !enabled {
        TEXT_DISABLED
    } else {
        mix(TEXT_SECONDARY, TEXT, t)
    };
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(1.0),
            Rounding::same(R_SMALL),
            Stroke::new(1.0_f32, ACCENT),
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon.glyph(),
        icon_font(ICON_SIZE_MD),
        color,
    );

    let response = if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    };
    response.on_hover_text(tooltip)
}

/// Small counted chip (e.g. the number of downloads).
pub fn chip(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = FontId::new(CAPTION_SIZE, proportional());
    let width = text_width(ui, text, font.clone()) + SP_S * 2.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, CHIP_H), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::same(10.0), mix(transparent(), color, 0.14));
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, text, font, color);
    response
}

/// Status chip with a colored dot, matching the job state.
pub fn status_chip(ui: &mut Ui, status: &JobStatus) -> Response {
    let (label, color) = status_style(status);
    let font = FontId::new(CAPTION_SIZE, proportional());
    let width = text_width(ui, label, font.clone()) + SP_S * 2.0 + 12.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, STATUS_CHIP_H), Sense::hover());

    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(STATUS_CHIP_H * 0.5), mix(transparent(), color, 0.12));
    if matches!(status, JobStatus::Completed) {
        painter.text(
            egui::pos2(rect.left() + SP_S + 4.0, rect.center().y),
            Align2::CENTER_CENTER,
            Icon::Check.glyph(),
            icon_font(ICON_SIZE_XS),
            color,
        );
    } else {
        painter.circle_filled(
            egui::pos2(rect.left() + SP_S + 3.0, rect.center().y),
            3.0,
            color,
        );
    }
    painter.text(
        egui::pos2(rect.left() + SP_S + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    response
}

/// Display label and color for a job status.
pub fn status_style(status: &JobStatus) -> (&'static str, Color32) {
    match status {
        JobStatus::Queued => ("Queued", NEUTRAL),
        JobStatus::Starting => ("Starting", ACCENT),
        JobStatus::Downloading => ("Downloading", ACCENT),
        JobStatus::Resolving => ("Resolving", ACCENT),
        JobStatus::Merging => ("Merging", ACCENT),
        JobStatus::Pausing => ("Pausing", WARNING),
        JobStatus::Paused => ("Paused", NEUTRAL),
        JobStatus::Completed => ("Done", SUCCESS),
        JobStatus::Failed => ("Error", ERROR),
        JobStatus::Cancelled => ("Stopped", NEUTRAL),
    }
}

// --- progress --------------------------------------------------------------

/// Slim animated progress bar. `id` must be stable across frames.
pub fn progress(ui: &mut Ui, id: Id, fraction: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), PROGRESS_H),
        Sense::hover(),
    );
    let target = fraction.clamp(0.0, 1.0);
    let shown = ui.ctx().animate_value_with_time(id, target, 0.3);
    let rounding = Rounding::same(PROGRESS_H * 0.5);

    ui.painter().rect_filled(rect, rounding, TRACK);
    if shown > 0.001 {
        let width = (rect.width() * shown).max(PROGRESS_H);
        let fill = Rect::from_min_size(rect.min, Vec2::new(width, rect.height()));
        ui.painter().rect_filled(fill, rounding, color);
    }
}

// --- feedback --------------------------------------------------------------

/// Tinted inline alert bar.
pub fn alert(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::default()
        .fill(mix(transparent(), color, 0.08))
        .stroke(Stroke::new(1.0_f32, mix(transparent(), color, 0.28)))
        .rounding(Rounding::same(R_SMALL))
        .inner_margin(egui::Margin::symmetric(SP_S, SP_XS + 2.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = SP_XS + 2.0;
                ui.add(egui::Label::new(
                    RichText::new(Icon::Info.glyph())
                        .font(icon_font(CAPTION_SIZE))
                        .color(color),
                ));
                ui.add(egui::Label::new(
                    RichText::new(text).font(FontId::new(CAPTION_SIZE, proportional())).color(color),
                ));
            });
        });
}

/// Small info line with an icon, used for helper text.
pub fn info_line(ui: &mut Ui, text: &str) {
    note_line(ui, text, TEXT_TERTIARY);
}

/// Inline warning note, used for "this will be replaced" style hints.
pub fn warning_line(ui: &mut Ui, text: &str) {
    note_line(ui, text, WARNING);
}

fn note_line(ui: &mut Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_XS + 2.0;
        ui.add(egui::Label::new(
            RichText::new(Icon::Info.glyph())
                .font(icon_font(CAPTION_SIZE))
                .color(color),
        ));
        ui.add(egui::Label::new(
            RichText::new(text)
                .font(FontId::new(CAPTION_SIZE, proportional()))
                .color(color),
        ));
    });
}

// --- inputs ----------------------------------------------------------------

pub struct Field<'a> {
    id: &'a str,
    hint: &'a str,
    icon: Option<Icon>,
    suffix: Option<&'a str>,
    invalid: bool,
    height: f32,
    width: Option<f32>,
}

impl<'a> Field<'a> {
    pub fn new(id: &'a str, hint: &'a str) -> Self {
        Self {
            id,
            hint,
            icon: None,
            suffix: None,
            invalid: false,
            height: FIELD_H,
            width: None,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn suffix(mut self, suffix: &'a str) -> Self {
        self.suffix = Some(suffix);
        self
    }

    pub fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }
}

pub struct FieldResponse {
    pub response: Response,
    /// Enter was pressed while the field had focus.
    pub submitted: bool,
}

/// Fluent text field: filled surface, accent focus border/underline, optional
/// leading icon and trailing suffix.
pub fn text_field(ui: &mut Ui, field: Field<'_>, buf: &mut String) -> FieldResponse {
    let width = field.width.unwrap_or_else(|| ui.available_width());

    ui.push_id(field.id, |ui| {
        let (rect, background) =
            ui.allocate_exact_size(Vec2::new(width, field.height), Sense::click());
        let painter = ui.painter().clone();
        painter.rect_filled(rect, Rounding::same(R_SMALL), FIELD);

        let icon_w = if field.icon.is_some() { FIELD_ICON_W } else { 0.0 };
        let suffix_w = field
            .suffix
            .map(|s| text_width(ui, s, FontId::new(CAPTION_SIZE, proportional())) + SP_S)
            .unwrap_or(0.0);

        let left = rect.left() + SP_M + icon_w;
        let right = rect.right() - SP_M - suffix_w;
        let text_rect = Rect::from_min_max(
            egui::pos2(left, rect.center().y - 11.0),
            egui::pos2(right.max(left + 24.0), rect.center().y + 11.0),
        );

        let response = ui.put(
            text_rect,
            egui::TextEdit::singleline(buf)
                .id(Id::new(field.id))
                .frame(false)
                .margin(egui::Margin::symmetric(0.0, 0.0))
                .vertical_align(Align::Center)
                .hint_text(field.hint)
                .text_color(TEXT),
        );

        if let Some(icon) = field.icon {
            painter.text(
                egui::pos2(rect.left() + SP_M + 7.0, rect.center().y),
                Align2::CENTER_CENTER,
                icon.glyph(),
                icon_font(ICON_SIZE_SM),
                TEXT_TERTIARY,
            );
        }
        if let Some(suffix) = field.suffix {
            painter.text(
                egui::pos2(rect.right() - SP_M, rect.center().y),
                Align2::RIGHT_CENTER,
                suffix,
                FontId::new(CAPTION_SIZE, proportional()),
                TEXT_TERTIARY,
            );
        }

        let focused = response.has_focus();
        let (color, width_px) = if field.invalid {
            (ERROR, 1.5)
        } else if focused {
            (ACCENT, 1.5)
        } else {
            (STROKE_SOFT, 1.0_f32)
        };
        painter.rect_stroke(rect, Rounding::same(R_SMALL), Stroke::new(width_px, color));
        if focused && !field.invalid {
            let y = rect.bottom() - 1.5;
            painter.line_segment(
                [
                    egui::pos2(rect.left() + R_SMALL, y),
                    egui::pos2(rect.right() - R_SMALL, y),
                ],
                Stroke::new(2.0_f32, ACCENT),
            );
        }

        if background.clicked() {
            response.request_focus();
        }

        let submitted = (response.has_focus() || response.lost_focus())
            && ui.input(|i| i.key_pressed(egui::Key::Enter));

        FieldResponse {
            response,
            submitted,
        }
    })
    .inner
}

/// Compact stepper: `-  value  +` with range clamping.
pub fn stepper(
    ui: &mut Ui,
    id: &str,
    value: &mut usize,
    range: std::ops::RangeInclusive<usize>,
) -> bool {
    let mut changed = false;
    ui.push_id(id, |ui| {
        ui.spacing_mut().item_spacing.x = SP_XS;
        ui.horizontal(|ui| {
            let minimum = *value <= *range.start();
            if icon_button(ui, Icon::Minus, "Decrease", !minimum).clicked() {
                *value = (*value).saturating_sub(1).max(*range.start());
                changed = true;
            }

            let (rect, _) = ui.allocate_exact_size(Vec2::new(STEP_VALUE_W, FIELD_H), Sense::hover());
            ui.painter()
                .rect_filled(rect, Rounding::same(R_SMALL), FIELD);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                value.to_string(),
                FontId::new(BODY_SIZE, proportional()),
                TEXT,
            );

            let maximum = *value >= *range.end();
            if icon_button(ui, Icon::Plus, "Increase", !maximum).clicked() {
                *value = (*value + 1).min(*range.end());
                changed = true;
            }
        });
    });
    changed
}

/// Animated expandable section with a chevron header.
pub fn collapsible(
    ui: &mut Ui,
    id_salt: &str,
    label: &str,
    default_open: bool,
    body: impl FnOnce(&mut Ui),
) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );

    let open = state.is_open();
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), CONTROL_H), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(id.with("hover"), response.hovered(), ANIM);
    if hover > 0.0 {
        ui.painter().rect_filled(
            rect,
            Rounding::same(R_SMALL),
            mix(transparent(), CONTROL_HOVER, hover),
        );
    }

    let chevron = if open {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            Rounding::same(R_SMALL),
            Stroke::new(1.0_f32, ACCENT),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + SP_S + 5.0, rect.center().y),
        Align2::CENTER_CENTER,
        chevron.glyph(),
        icon_font(CAPTION_SIZE),
        TEXT_SECONDARY,
    );
    ui.painter().text(
        egui::pos2(rect.left() + SP_S + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::new(BODY_SIZE, proportional()),
        TEXT,
    );

    if response.clicked() {
        state.toggle(ui);
    }
    let _ = response.on_hover_cursor(CursorIcon::PointingHand);

    state.show_body_unindented(ui, |ui| {
        ui.add_space(SP_S);
        ui.indent(id, |ui| {
            ui.expand_to_include_x(rect.right());
            body(ui);
        });
    });
}

/// Centered empty placeholder.
pub fn empty_state(ui: &mut Ui, icon: Icon, title: &str, subtitle: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(SP_L);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(EMPTY_ICON_BOX), Sense::hover());
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon.glyph(),
            icon_font(ICON_SIZE_XL),
            mix(transparent(), TEXT_TERTIARY, 0.9),
        );
        ui.add_space(SP_S);
        ui.add(egui::Label::new(
            RichText::new(title)
                .font(FontId::new(SECTION_SIZE, semibold_family()))
                .color(TEXT_SECONDARY),
        ));
        ui.add_space(SP_XS);
        ui.add(egui::Label::new(caption_tertiary(subtitle)));
        ui.add_space(SP_M);
    });
}
