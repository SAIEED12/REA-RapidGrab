//! Application logo: embedded once, exposed as a window/taskbar icon and as an
//! in-window texture for the header.

use std::sync::OnceLock;

use eframe::egui;

/// 1024x1024 master logo. Regenerate with `tools/make-logo.ps1`.
pub const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");

/// Decoded RGBA8 pixels plus dimensions.
pub struct Logo {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn logo() -> &'static Logo {
    static LOGO: OnceLock<Logo> = OnceLock::new();
    LOGO.get_or_init(|| {
        let (rgba, width, height) =
            decode_png(LOGO_PNG).expect("assets/logo.png must be a valid PNG");
        Logo {
            rgba,
            width,
            height,
        }
    })
}

/// Icon for the title bar, taskbar and Alt-Tab switcher.
pub fn window_icon() -> egui::IconData {
    let logo = logo();
    egui::IconData {
        rgba: logo.rgba.clone(),
        width: logo.width,
        height: logo.height,
    }
}

/// Texture for drawing the logo inside the window; load once per context.
pub fn logo_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let logo = logo();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [logo.width as usize, logo.height as usize],
        &logo.rgba,
    );
    ctx.load_texture("rapidgrab-logo", image, egui::TextureOptions::LINEAR)
}

/// Decodes a PNG into unmultiplied RGBA8, normalising other color types.
fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or_default()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let pixels = &buf[..info.buffer_size()];

    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 0xff]).collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return Err("indexed PNG was not expanded to RGB".to_string()),
    };

    Ok((rgba, info.width, info.height))
}
