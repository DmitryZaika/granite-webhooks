//! Thumbnails of uploaded images.
//!
//! In the picture's own shape, just big enough to cover a 480 x 320 tile
//! (tiles crop it with `object-fit: cover`; it is never stretched), upright
//! per EXIF orientation, HEIC included. A JPEG, or a PNG when at least 1% of
//! the picture is see-through (a cutout, not a photo's clear corners).

use crate::images::{self, encode_jpeg, encode_png};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use std::io::Cursor;

pub const ICON_WIDTH: u32 = 480;
pub const ICON_HEIGHT: u32 = 320;
/// Longest side of a thumbnail: very long pictures stop here.
const ICON_MAX_SIDE: u32 = 960;
const ICON_JPEG_QUALITY: u8 = 80;

/// A thumbnail, ready to store.
pub struct Icon {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

/// The thumbnail, or `None` when the bytes are not a picture this can read
/// (the upload is kept without one, as in Remix). CPU work: call it from
/// `tokio::task::spawn_blocking`.
pub fn make_icon(bytes: &[u8]) -> Option<Icon> {
    let image = images::decode(bytes).ok()?;
    let (width, height) = icon_size(image.width(), image.height());
    let icon = if image.width() / 4 > width {
        // Box-averaged first: Lanczos3 over a 36 MP photo is slow and no sharper.
        image.thumbnail_exact(width * 2, height * 2).resize_exact(
            width,
            height,
            FilterType::Lanczos3,
        )
    } else {
        image.resize_exact(width, height, FilterType::Lanczos3)
    };
    Some(if has_see_through_parts(&icon) {
        Icon {
            bytes: encode_png(&icon),
            content_type: "image/png",
        }
    } else {
        // Any clear pixels turn white.
        Icon {
            bytes: encode_jpeg(&icon, ICON_JPEG_QUALITY),
            content_type: "image/jpeg",
        }
    })
}

/// Whether at least 1% of the picture is see-through.
fn has_see_through_parts(image: &DynamicImage) -> bool {
    if !image.color().has_alpha() {
        return false;
    }
    let rgba = image.to_rgba8();
    let clear = rgba.pixels().filter(|pixel| pixel.0[3] < u8::MAX).count();
    let total = u64::from(rgba.width()) * u64::from(rgba.height());
    u64::try_from(clear).unwrap_or(u64::MAX) * 100 >= total
}

/// The thumbnail's size: the picture's shape, just covering 480 x 320, its
/// longest side at most 960, never enlarged.
fn icon_size(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = (u64::from(width.max(1)), u64::from(height.max(1)));
    // `side * num / den`, rounded, at least 1.
    let scaled = |side: u64, num: u64, den: u64| {
        u32::try_from(((side * num + den / 2) / den).max(1)).unwrap_or(u32::MAX)
    };
    // Wider than 3:2, the height covers the tile; otherwise the width does.
    let (cover_w, cover_h) = if w * u64::from(ICON_HEIGHT) >= h * u64::from(ICON_WIDTH) {
        (scaled(w, u64::from(ICON_HEIGHT), h), ICON_HEIGHT)
    } else {
        (ICON_WIDTH, scaled(h, u64::from(ICON_WIDTH), w))
    };
    let (w1, h1) = if cover_w >= width || cover_h >= height {
        (width.max(1), height.max(1))
    } else {
        (cover_w, cover_h)
    };
    let long = u64::from(w1.max(h1));
    if long <= u64::from(ICON_MAX_SIDE) {
        return (w1, h1);
    }
    let max = u64::from(ICON_MAX_SIDE);
    (
        scaled(u64::from(w1), max, long),
        scaled(u64::from(h1), max, long),
    )
}

/// Image content types uploads accept.
pub const IMAGE_CONTENT_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/heic",
    "image/heif",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::new_rgb8(width, height);
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn size_of(icon: &Icon) -> (u32, u32) {
        let decoded = image::load_from_memory(&icon.bytes).unwrap();
        (decoded.width(), decoded.height())
    }

    #[test]
    fn icons_keep_the_picture_shape_and_cover_480_by_320() {
        assert_eq!(size_of(&make_icon(&png(1000, 1000)).unwrap()), (480, 480));
        assert_eq!(size_of(&make_icon(&png(1920, 1080)).unwrap()), (569, 320));
        // A phone photo held upright.
        assert_eq!(size_of(&make_icon(&png(1440, 1920)).unwrap()), (480, 640));
        assert_eq!(icon_size(4032, 3024), (480, 360));
        // A scrolling screenshot stops at 960 on its long side.
        assert_eq!(icon_size(1130, 5797), (187, 960));
        assert_eq!(icon_size(300, 3000), (96, 960));
        // Small pictures are never enlarged.
        assert_eq!(icon_size(64, 48), (64, 48));
    }

    #[test]
    fn photos_become_jpeg_and_see_through_pictures_png() {
        let icon = make_icon(&png(1000, 800)).unwrap();
        assert_eq!(icon.content_type, "image/jpeg");
        assert_eq!(image::guess_format(&icon.bytes).unwrap(), ImageFormat::Jpeg);
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            900,
            600,
            image::Rgba([10, 20, 30, 0]),
        ))
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
        let icon = make_icon(out.get_ref()).unwrap();
        assert_eq!(icon.content_type, "image/png");
        assert_eq!(size_of(&icon), (480, 320));
        // A photo with clear corners is still a photo.
        let mut photo = image::RgbaImage::from_pixel(900, 600, image::Rgba([200, 180, 160, 255]));
        for (x, y) in [(0, 0), (899, 0), (0, 599), (899, 599)] {
            photo.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(photo)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        assert_eq!(make_icon(out.get_ref()).unwrap().content_type, "image/jpeg");
    }

    #[test]
    fn heic_photos_have_icons() {
        let icon = make_icon(include_bytes!("../../../tests/fixtures/photo-640x480.heic")).unwrap();
        assert_eq!(icon.content_type, "image/jpeg");
        assert_eq!(size_of(&icon), (480, 360));
    }

    #[test]
    fn non_images_have_no_icon() {
        assert!(make_icon(b"%PDF-1.4 not an image").is_none());
        assert!(make_icon(b"").is_none());
    }
}

/// The image without its blank border (`sharp(...).rotate().trim()`).
///
/// Rows and columns whose every pixel is within 12 of the top-left pixel
/// are cut off, after turning the image upright. PNG and JPEG only; `None`
/// when there is nothing to cut or the bytes are not one of those.
pub fn trim_border(bytes: &[u8]) -> Option<Vec<u8>> {
    const THRESHOLD: u8 = 12;
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return None;
    }
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let pixels = image.to_rgba8();
    let (width, height) = pixels.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let background = *pixels.get_pixel(0, 0);
    let differs = |x: u32, y: u32| {
        let pixel = pixels.get_pixel(x, y);
        pixel
            .0
            .iter()
            .zip(background.0.iter())
            .any(|(a, b)| a.abs_diff(*b) > THRESHOLD)
    };
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if differs(x, y) {
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
    }
    if left > right || top > bottom {
        return None;
    }
    if left == 0 && top == 0 && right == width - 1 && bottom == height - 1 {
        return None;
    }
    let trimmed = image.crop_imm(left, top, right - left + 1, bottom - top + 1);
    let trimmed = match format {
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(trimmed.to_rgb8()),
        _ => trimmed,
    };
    let mut out = Cursor::new(Vec::new());
    trimmed.write_to(&mut out, format).ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod trim_tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn a_blank_border_is_cut_off() {
        let mut canvas = RgbaImage::from_pixel(40, 30, Rgba([255, 255, 255, 255]));
        for x in 10..20 {
            for y in 5..15 {
                canvas.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(canvas)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let trimmed = trim_border(png.get_ref()).unwrap();
        let image = image::load_from_memory(&trimmed).unwrap();
        assert_eq!((image.width(), image.height()), (10, 10));
        // Nothing left to cut.
        assert!(trim_border(&trimmed).is_none());
    }
}
