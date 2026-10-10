//! Thumbnails of uploaded images.
//!
//! 480 x 320, stretched to fill, upright per EXIF orientation, in the
//! original format. Port of `compressImage`
//! (sharp `.rotate().resize(480, 320, { fit: 'fill' })`) in the Remix app.

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use std::io::Cursor;

pub const ICON_WIDTH: u32 = 480;
pub const ICON_HEIGHT: u32 = 320;

/// The thumbnail, or `None` when the bytes are not an image this can read
/// (the upload is kept without one, as in Remix).
pub fn make_icon(bytes: &[u8]) -> Option<Vec<u8>> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let icon = image.resize_exact(ICON_WIDTH, ICON_HEIGHT, FilterType::Triangle);
    let icon = match format {
        // JPEG has no alpha channel.
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(icon.to_rgb8()),
        _ => icon,
    };
    let mut out = Cursor::new(Vec::new());
    icon.write_to(&mut out, format).ok()?;
    Some(out.into_inner())
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

    #[test]
    fn icons_are_480_by_320_in_the_same_format() {
        let icon = make_icon(&png(1000, 1000)).unwrap();
        let decoded = image::load_from_memory(&icon).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (480, 320));
        assert_eq!(image::guess_format(&icon).unwrap(), ImageFormat::Png);
    }

    #[test]
    fn jpeg_stays_jpeg() {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(64, 48)
            .write_to(&mut out, ImageFormat::Jpeg)
            .unwrap();
        let icon = make_icon(&out.into_inner()).unwrap();
        assert_eq!(image::guess_format(&icon).unwrap(), ImageFormat::Jpeg);
    }

    #[test]
    fn non_images_have_no_icon() {
        assert!(make_icon(b"%PDF-1.4 not an image").is_none());
        assert!(make_icon(b"").is_none());
    }
}
