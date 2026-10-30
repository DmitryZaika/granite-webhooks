//! A picture of a PDF's first page, so lists can show what a document is.
//!
//! Rendered with `hayro` (pure Rust) at 400 px wide (at most 1200 px tall),
//! on white, as a lossless WebP. The CRM drew it with pdf.js and sharp
//! (lossy WebP at quality 80). CPU work: call from `spawn_blocking`.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};
use std::sync::Arc;

pub const PREVIEW_WIDTH_PX: f32 = 400.0;
pub const PREVIEW_MAX_HEIGHT_PX: f32 = 1200.0;

/// The first page as a WebP; `None` when the file cannot be read as a PDF
/// (or has no pages).
pub fn first_page_webp(pdf: Vec<u8>) -> Option<Vec<u8>> {
    let pdf = Pdf::new(Arc::new(pdf)).ok()?;
    let page = pdf.pages().first()?;
    let (width, height) = page.render_dimensions();
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (PREVIEW_WIDTH_PX / width).min(PREVIEW_MAX_HEIGHT_PX / height);
    let pixmap = render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    let (w, h) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
    if w == 0 || h == 0 {
        return None;
    }
    // The background is opaque white, so premultiplied = straight alpha;
    // drop alpha for a smaller file.
    let rgb: Vec<u8> = pixmap
        .data_as_u8_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_renders_400_wide() {
        let mut page = crate::pdf::Page::new();
        page.text("Quote for Alice", 72.0, 700.0, 24.0, crate::pdf::Font::Bold);
        page.rectangle(72.0, 500.0, 300.0, 100.0, 4.0);
        let pdf = page.finish();
        let webp = first_page_webp(pdf).unwrap();
        let image = image::load_from_memory(&webp).unwrap();
        assert_eq!(image.width(), 400);
        // 841.89 pt * 400 / 595.28, truncated.
        assert_eq!(image.height(), 565);
        // Mostly white paper with some dark text.
        let rgb = image.to_rgb8();
        let dark = rgb
            .pixels()
            .filter(|p| p.0.iter().all(|c| *c < 100))
            .count();
        assert!(dark > 50, "{dark} dark pixels");
        assert!(first_page_webp(b"not a pdf".to_vec()).is_none());
    }
}
