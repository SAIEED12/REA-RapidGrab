//! Design tokens and the Fluent-inspired egui theme.
//!
//! Every color, radius, spacing step and font size used by the UI lives here.

use std::sync::OnceLock;

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText,
};

// --- surfaces --------------------------------------------------------------

/// Window background (Mica-like base).
pub const BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
/// Card / layer fill.
pub const CARD: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2B);
/// Control fill (`#FFFFFF0F` over a card).
pub const CONTROL: Color32 = Color32::from_rgb(0x37, 0x37, 0x37);
/// Control hover (`#FFFFFF17` over a card).
pub const CONTROL_HOVER: Color32 = Color32::from_rgb(0x40, 0x40, 0x40);
/// Control pressed (`#FFFFFF0A` over a card).
pub const CONTROL_PRESSED: Color32 = Color32::from_rgb(0x32, 0x32, 0x32);
/// Text input fill.
pub const FIELD: Color32 = Color32::from_rgb(0x37, 0x37, 0x37);
/// Card / control border (`#FFFFFF14`).
pub const STROKE: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
/// Softer border for nested layers.
pub const STROKE_SOFT: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2A);
/// Visible empty-track fill (~#FFFFFF1F over CARD).
pub const TRACK_FILL: Color32 = Color32::from_rgb(0x4A, 0x4A, 0x4A);

// --- text ------------------------------------------------------------------

pub const TEXT: Color32 = Color32::WHITE;
/// `#FFFFFFC5`
pub const TEXT_SECONDARY: Color32 = Color32::from_rgba_premultiplied(0xC5, 0xC5, 0xC5, 0xC5);
/// `#FFFFFF8B`
pub const TEXT_TERTIARY: Color32 = Color32::from_rgba_premultiplied(0x8B, 0x8B, 0x8B, 0x8B);
/// `#FFFFFF5C`
pub const TEXT_DISABLED: Color32 = Color32::from_rgba_premultiplied(0x5C, 0x5C, 0x5C, 0x5C);

// --- accent and status -----------------------------------------------------

pub const ACCENT: Color32 = Color32::from_rgb(0x60, 0xCD, 0xFF);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x78, 0xD4, 0xFF);
pub const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x4C, 0xB4, 0xE6);
/// `#000000E6`
pub const ON_ACCENT: Color32 = Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0xE6);

pub const SUCCESS: Color32 = Color32::from_rgb(0x6C, 0xCB, 0x5F);
/// Quieter green used for completed progress fills.
pub const SUCCESS_MUTED: Color32 = Color32::from_rgb(0x4F, 0x94, 0x47);
pub const WARNING: Color32 = Color32::from_rgb(0xFC, 0xE1, 0x00);
pub const ERROR: Color32 = Color32::from_rgb(0xFF, 0x99, 0xA4);
/// Paused / neutral status.
pub const NEUTRAL: Color32 = TEXT_TERTIARY;

// --- geometry --------------------------------------------------------------

pub const R_SMALL: f32 = 4.0;
pub const R_MEDIUM: f32 = 8.0;
pub const R_CARD: f32 = 12.0;

pub const SP_XS: f32 = 4.0;
pub const SP_S: f32 = 8.0;
pub const SP_M: f32 = 12.0;
pub const SP_L: f32 = 16.0;
pub const SP_XL: f32 = 24.0;

pub const CONTROL_H: f32 = 32.0;
pub const FIELD_H: f32 = 36.0;
/// Taller field used for the primary URL input.
pub const FIELD_TALL_H: f32 = 40.0;
pub const BTN_H: f32 = 36.0;
pub const ICON_BTN: f32 = 28.0;
pub const PROGRESS_H: f32 = 6.0;

pub const BUTTON_MIN_W: f32 = 132.0;
/// Browse button width, sized for its label.
pub const BROWSE_W: f32 = 104.0;
/// Speed-cap field width.
pub const LIMIT_W: f32 = 128.0;
/// Numeric value box inside a stepper.
pub const STEP_VALUE_W: f32 = 44.0;

pub const CHIP_H: f32 = 20.0;
pub const STATUS_CHIP_H: f32 = 22.0;
/// Fluent checkbox box edge.
pub const CHECKBOX_SIZE: f32 = 20.0;
/// Clickable height of a checkbox row.
pub const CHECKBOX_ROW_H: f32 = 32.0;
/// Width of the compact Max-height dropdown.
pub const DROPDOWN_W: f32 = 160.0;
/// Below this content width, form fields collapse to a single column.
pub const FORM_COLUMN_MIN: f32 = 560.0;
/// Square reserved for the large empty-state glyph.
pub const EMPTY_ICON_BOX: f32 = 30.0;
/// Leading icon gutter inside a text field.
pub const FIELD_ICON_W: f32 = 22.0;

// Icon glyph sizes, in points.
pub const ICON_SIZE_XS: f32 = 9.0;
pub const ICON_SIZE_SM: f32 = 13.0;
pub const ICON_SIZE_MD: f32 = 14.0;
pub const ICON_SIZE_XL: f32 = 26.0;

/// Maximum content width, centered in wide windows.
pub const CONTENT_W: f32 = 760.0;

/// Shared hover/collapse animation time (seconds).
pub const ANIM: f32 = 0.14;

// --- typography ------------------------------------------------------------

pub const TITLE_SIZE: f32 = 20.0;
pub const SECTION_SIZE: f32 = 16.0;
pub const BODY_SIZE: f32 = 14.0;
pub const CAPTION_SIZE: f32 = 12.0;
/// Form-field label size.
pub const LABEL_SIZE: f32 = 13.0;

const SEMIBOLD_FAMILY: &str = "rg-semibold";
const ICON_FAMILY: &str = "rg-icons";

static ICONS_AVAILABLE: OnceLock<bool> = OnceLock::new();

pub fn proportional() -> FontFamily {
    FontFamily::Proportional
}

pub fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD_FAMILY.into())
}

pub fn icon_family() -> FontFamily {
    FontFamily::Name(ICON_FAMILY.into())
}

/// Whether a system icon font was found during [`apply_theme`].
pub fn icons_available() -> bool {
    ICONS_AVAILABLE.get().copied().unwrap_or(false)
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, if icons_available() { icon_family() } else { proportional() })
}

pub fn title(text: &str) -> RichText {
    RichText::new(text)
        .font(FontId::new(TITLE_SIZE, semibold_family()))
        .color(TEXT)
}

pub fn section(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(FontId::new(SECTION_SIZE, semibold_family()))
        .color(TEXT)
}

pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(FontId::new(CAPTION_SIZE, proportional()))
        .color(TEXT_SECONDARY)
}

/// 13px caption for form-field labels, slightly brighter than a caption.
pub fn label(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(FontId::new(LABEL_SIZE, proportional()))
        .color(TEXT)
}

pub fn caption_tertiary(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(FontId::new(CAPTION_SIZE, proportional()))
        .color(TEXT_TERTIARY)
}

// --- icons -----------------------------------------------------------------

/// Icons drawn from the system icon font, with a plain-text fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Download,
    Link,
    Pause,
    Play,
    Stop,
    Remove,
    Folder,
    OpenFile,
    Retry,
    Info,
    Check,
    Plus,
    Minus,
    ChevronDown,
    ChevronRight,
}

impl Icon {
    /// Glyph to draw; falls back to readable text when no icon font exists.
    pub fn glyph(self) -> &'static str {
        if icons_available() {
            self.fluent()
        } else {
            self.plain()
        }
    }

    fn fluent(self) -> &'static str {
        match self {
            Self::Download => "\u{E896}",
            Self::Link => "\u{E71B}",
            Self::Pause => "\u{E769}",
            Self::Play => "\u{E768}",
            Self::Stop => "\u{E711}",
            Self::Remove => "\u{E74D}",
            Self::Folder => "\u{E8B7}",
            Self::OpenFile => "\u{E8E5}",
            Self::Retry => "\u{E72C}",
            Self::Info => "\u{E946}",
            Self::Check => "\u{E73E}",
            Self::Plus => "\u{E710}",
            Self::Minus => "\u{E738}",
            Self::ChevronDown => "\u{E70D}",
            Self::ChevronRight => "\u{E76C}",
        }
    }

    fn plain(self) -> &'static str {
        match self {
            Self::Download => "v",
            Self::Link => "@",
            Self::Pause => "II",
            Self::Play => ">",
            Self::Stop => "x",
            Self::Remove => "x",
            Self::Folder => "[]",
            Self::OpenFile => "->",
            Self::Retry => "R",
            Self::Info => "i",
            Self::Check => "ok",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::ChevronDown => "v",
            Self::ChevronRight => ">",
        }
    }
}

// --- theme application -----------------------------------------------------

fn font_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string()),
    )
    .join("Fonts")
}

fn read_font(names: &[&str]) -> Option<FontData> {
    let dir = font_dir();
    for name in names {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            return Some(FontData::from_owned(bytes));
        }
    }
    None
}

/// Installs Segoe UI / Segoe UI Semibold / Segoe MDL2 Assets when present,
/// keeping egui's bundled fonts as the fallback chain.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let regular = read_font(&["SegUIVar.ttf", "segoeui.ttf"]);
    let semibold = read_font(&["SegUIVar.ttf", "seguisb.ttf", "segoeuib.ttf"]);

    if let Some(data) = regular {
        fonts.font_data.insert("rg-ui".to_string(), data);
        if let Some(chain) = fonts.families.get_mut(&FontFamily::Proportional) {
            chain.insert(0, "rg-ui".to_string());
        }
    }

    let base_chain = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let semibold_chain = if let Some(data) = semibold {
        fonts.font_data.insert("rg-semibold".to_string(), data);
        let mut chain = vec!["rg-semibold".to_string()];
        chain.extend(base_chain.iter().cloned());
        chain
    } else {
        base_chain.clone()
    };
    fonts.families.insert(semibold_family(), semibold_chain);

    let icons = read_font(&["Segoe Fluent Icons.ttf", "segmdl2.ttf"]);
    let has_icons = icons.is_some();
    if let Some(data) = icons {
        fonts.font_data.insert("rg-icons".to_string(), data);
        fonts
            .families
            .insert(icon_family(), vec!["rg-icons".to_string()]);
    }
    let _ = ICONS_AVAILABLE.set(has_icons);

    ctx.set_fonts(fonts);
}

fn shadow() -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: egui::vec2(0.0, 6.0),
        blur: 24.0,
        spread: 0.0,
        color: Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0x50),
    }
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = None;

    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = egui::Stroke::new(1.0_f32, STROKE);
    v.window_rounding = egui::Rounding::same(R_CARD);
    v.menu_rounding = egui::Rounding::same(R_MEDIUM);
    v.window_shadow = shadow();
    v.popup_shadow = shadow();
    v.window_highlight_topmost = false;

    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.code_bg_color = FIELD;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARNING;
    v.error_fg_color = ERROR;

    v.selection.bg_fill = Color32::from_rgba_premultiplied(0x30, 0x66, 0x7F, 0x80);
    v.selection.stroke = egui::Stroke::new(1.0_f32, TEXT);
    v.text_cursor.stroke = egui::Stroke::new(1.0_f32, ACCENT);

    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.slider_trailing_fill = true;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.image_loading_spinners = false;

    let rounding = egui::Rounding::same(R_MEDIUM);

    let mut noninteractive = v.widgets.noninteractive;
    noninteractive.bg_fill = CARD;
    noninteractive.weak_bg_fill = CARD;
    noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, STROKE_SOFT);
    noninteractive.rounding = egui::Rounding::same(R_SMALL);
    noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT_SECONDARY);
    noninteractive.expansion = 0.0;
    v.widgets.noninteractive = noninteractive;

    let mut inactive = v.widgets.inactive;
    inactive.bg_fill = CONTROL;
    inactive.weak_bg_fill = CONTROL;
    inactive.bg_stroke = egui::Stroke::new(1.0_f32, STROKE_SOFT);
    inactive.rounding = rounding;
    inactive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    inactive.expansion = 0.0;
    v.widgets.inactive = inactive;

    let mut hovered = v.widgets.hovered;
    hovered.bg_fill = CONTROL_HOVER;
    hovered.weak_bg_fill = CONTROL_HOVER;
    hovered.bg_stroke = egui::Stroke::new(1.0_f32, STROKE);
    hovered.rounding = rounding;
    hovered.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    hovered.expansion = 0.0;
    v.widgets.hovered = hovered;

    let mut active = v.widgets.active;
    active.bg_fill = CONTROL_PRESSED;
    active.weak_bg_fill = CONTROL_PRESSED;
    active.bg_stroke = egui::Stroke::new(1.0_f32, STROKE);
    active.rounding = rounding;
    active.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    active.expansion = 0.0;
    v.widgets.active = active;

    let mut open = v.widgets.open;
    open.bg_fill = CONTROL;
    open.weak_bg_fill = CONTROL;
    open.bg_stroke = egui::Stroke::new(1.0_f32, STROKE_SOFT);
    open.rounding = rounding;
    open.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    open.expansion = 0.0;
    v.widgets.open = open;

    v
}

fn style() -> egui::Style {
    let mut style = egui::Style {
        visuals: visuals(),
        animation_time: ANIM,
        ..Default::default()
    };

    style.spacing.item_spacing = egui::vec2(SP_S, SP_S);
    style.spacing.button_padding = egui::vec2(SP_M, SP_S);
    style.spacing.window_margin = egui::Margin::same(SP_L);
    style.spacing.menu_margin = egui::Margin::same(SP_S);
    style.spacing.indent = SP_L;
    style.spacing.slider_width = 120.0;
    style.spacing.interact_size = egui::vec2(40.0, CONTROL_H);
    style.spacing.icon_width = 12.0;
    style.spacing.icon_spacing = SP_S;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.bar_inner_margin = 2.0;
    // Reserved (non-overlay) scroll bars: an overlay bar covers the last few
    // pixels of the content area, clipping card borders and right-aligned
    // buttons when the window is at its minimum width.
    style.spacing.scroll.floating = false;

    style.text_styles = [
        (
            egui::TextStyle::Heading,
            FontId::new(SECTION_SIZE, semibold_family()),
        ),
        (egui::TextStyle::Body, FontId::new(BODY_SIZE, proportional())),
        (egui::TextStyle::Button, FontId::new(BODY_SIZE, proportional())),
        (egui::TextStyle::Small, FontId::new(CAPTION_SIZE, proportional())),
        (egui::TextStyle::Monospace, FontId::monospace(BODY_SIZE - 1.0)),
        (
            egui::TextStyle::Name("Title".into()),
            FontId::new(TITLE_SIZE, semibold_family()),
        ),
        (
            egui::TextStyle::Name("Section".into()),
            FontId::new(SECTION_SIZE, semibold_family()),
        ),
        (
            egui::TextStyle::Name("Caption".into()),
            FontId::new(CAPTION_SIZE, proportional()),
        ),
    ]
    .into();

    style
}

/// Applies fonts, visuals and style to the context.
pub fn apply_theme(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_style(style());
}
