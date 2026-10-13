//! Reading and re-encoding pictures people upload or receive: phone photos
//! (JPEG, PNG, GIF, WebP and iPhone HEIC), upright per their orientation.
//!
//! HEIC is decoded with `zune-heic` (pure Rust, so nothing to install on
//! Lambda); the CRM used `heic-convert` and sharp. Everything here is CPU
//! work: call it from `tokio::task::spawn_blocking`.

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{
    AnimationDecoder, DynamicImage, Frame, ImageDecoder, ImageEncoder, ImageReader, Rgba, RgbaImage,
};
use std::io::Cursor;
use zune_heic::HeifDecoder;
use zune_heic::zune_core::bytestream::ZCursor;
use zune_heic::zune_core::colorspace::ColorSpace;
use zune_heic::zune_core::options::DecoderOptions;

/// Decompression-bomb ceiling for decoded pictures (8192 x 8192).
pub const MAX_PIXELS: u64 = 8192 * 8192;
/// Biggest HEIC file read (the decoder's own default is 16 MB).
const MAX_HEIC_BYTES: usize = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// Not a picture this can read.
    #[error("unsupported_image")]
    Unsupported,
    /// More pixels than [`MAX_PIXELS`].
    #[error("image_too_large")]
    TooLarge,
    #[error("{0}")]
    Decode(String),
}

/// The picture types told apart by their first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    WebP,
    Heic,
    Avif,
}

impl Kind {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
            Self::Heic => "image/heic",
            Self::Avif => "image/avif",
        }
    }
}

/// What the bytes are, by their signature (like the `file-type` package).
pub fn sniff(bytes: &[u8]) -> Option<Kind> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Kind::Jpeg);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Kind::Png);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Kind::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(Kind::WebP);
    }
    // ISO base media: `....ftyp<brand>`, then compatible brands.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let end = size.clamp(12, bytes.len());
        let brands: Vec<&[u8]> = std::iter::once(&bytes[8..12])
            .chain(
                bytes[16.min(end)..end]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(<[u8; 4]>::as_slice),
            )
            .collect();
        if brands.iter().any(|b| matches!(*b, b"avif" | b"avis")) && bytes[8..12] != *b"heic" {
            return Some(Kind::Avif);
        }
        if brands.iter().any(|b| {
            matches!(
                *b,
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1"
            )
        }) {
            return Some(Kind::Heic);
        }
    }
    None
}

const fn check_size(width: u64, height: u64) -> Result<(), ImageError> {
    if width.saturating_mul(height) > MAX_PIXELS {
        Err(ImageError::TooLarge)
    } else {
        Ok(())
    }
}

/// A HEIC/HEIF photo's pixels, rotated and mirrored as the file says.
pub fn decode_heic(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    let options = DecoderOptions::default()
        .hvec_set_use_videotoolbox(false)
        .set_hevc_max_mdat_size(MAX_HEIC_BYTES)
        .set_max_width(16384)
        .set_max_height(16384);
    let mut decoder = HeifDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|err| ImageError::Decode(format!("{err:?}")))?;
    let (width, height) = (decoder.width().unwrap_or(0), decoder.height().unwrap_or(0));
    check_size(width as u64, height as u64)?;
    let pixels = decoder
        .decode()
        .map_err(|err| ImageError::Decode(format!("{err:?}")))?;
    let (width, height) = (
        u32::try_from(decoder.width().unwrap_or(width)).map_err(|_| ImageError::TooLarge)?,
        u32::try_from(decoder.height().unwrap_or(height)).map_err(|_| ImageError::TooLarge)?,
    );
    let bad = || ImageError::Decode("unexpected HEIC pixel buffer".into());
    Ok(match decoder.colorspace() {
        Some(ColorSpace::RGB) => image::RgbImage::from_raw(width, height, pixels)
            .ok_or_else(bad)?
            .into(),
        Some(ColorSpace::RGBA) => RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(bad)?
            .into(),
        Some(ColorSpace::Luma) => image::GrayImage::from_raw(width, height, pixels)
            .ok_or_else(bad)?
            .into(),
        Some(ColorSpace::LumaA) => image::GrayAlphaImage::from_raw(width, height, pixels)
            .ok_or_else(bad)?
            .into(),
        other => return Err(ImageError::Decode(format!("HEIC colorspace {other:?}"))),
    })
}

/// Any supported still picture (the first frame of a GIF), upright.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    match sniff(bytes) {
        Some(Kind::Heic) => decode_heic(bytes),
        Some(Kind::Avif) | None => Err(ImageError::Unsupported),
        Some(_) => {
            let reader = ImageReader::new(Cursor::new(bytes))
                .with_guessed_format()
                .map_err(|err| ImageError::Decode(err.to_string()))?;
            let mut decoder = reader
                .into_decoder()
                .map_err(|err| ImageError::Decode(err.to_string()))?;
            let (width, height) = decoder.dimensions();
            check_size(u64::from(width), u64::from(height))?;
            let orientation = decoder.orientation().ok();
            let mut image = DynamicImage::from_decoder(decoder)
                .map_err(|err| ImageError::Decode(err.to_string()))?;
            if let Some(orientation) = orientation {
                image.apply_orientation(orientation);
            }
            Ok(image)
        }
    }
}

/// Transparent areas on white (JPEG has no alpha).
fn flatten(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let Rgba([r, g, b, a]) = *rgba.get_pixel(x, y);
        let over = |c: u8| {
            let alpha = u16::from(a);
            u8::try_from((u16::from(c) * alpha + 255 * (255 - alpha) + 127) / 255).unwrap_or(255)
        };
        image::Rgb([over(r), over(g), over(b)])
    })
}

pub fn encode_jpeg(image: &DynamicImage, quality: u8) -> Vec<u8> {
    let rgb = flatten(image);
    let mut out = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut out, quality);
    // Encoding an in-memory RGB buffer cannot fail.
    let _ = encoder.write_image(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    );
    out
}

fn encode_png(image: &DynamicImage) -> Vec<u8> {
    let mut out = Vec::new();
    let rgba = image.to_rgba8();
    let _ = PngEncoder::new(&mut out).write_image(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        image::ExtendedColorType::Rgba8,
    );
    out
}

/// Scaled to fit `max x max`, never enlarged.
fn fit_inside(image: &DynamicImage, max: u32) -> DynamicImage {
    if image.width() <= max && image.height() <= max {
        return image.clone();
    }
    image.resize(max, max, FilterType::Lanczos3)
}

/// A HEIC photo as a JPEG (the CRM's `convertHeicToJpeg`, quality 92).
pub fn heic_to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    Ok(encode_jpeg(&decode_heic(bytes)?, 92))
}

/// A small JPEG for chat tiles: fits 320 x 320, never enlarged, quality 75
/// (the CRM's `createImageThumbnail`).
pub fn thumbnail_jpeg(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    Ok(encode_jpeg(&fit_inside(&decode(bytes)?, 320), 75))
}

/// Byte budget of a picture sent by text.
pub const SMS_IMAGE_TARGET_BYTES: usize = 480 * 1024;
/// Longest side of a picture sent by text.
pub const SMS_IMAGE_MAX_DIMENSION: u32 = 1600;

/// A picture ready to send by text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsImage {
    pub bytes: Vec<u8>,
    /// `image/jpeg`, `image/png` or `image/gif`.
    pub content_type: &'static str,
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
}

fn decode_gif_frames(bytes: &[u8]) -> Result<Vec<Frame>, ImageError> {
    let decoder =
        GifDecoder::new(Cursor::new(bytes)).map_err(|err| ImageError::Decode(err.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_size(u64::from(width), u64::from(height))?;
    decoder
        .into_frames()
        .collect_frames()
        .map_err(|err| ImageError::Decode(err.to_string()))
}

fn encode_gif(frames: &[Frame], max: u32) -> Result<(Vec<u8>, u32, u32), ImageError> {
    let mut out = Vec::new();
    let mut size = (0, 0);
    {
        let mut encoder = GifEncoder::new_with_speed(&mut out, 10);
        encoder
            .set_repeat(Repeat::Infinite)
            .map_err(|err| ImageError::Decode(err.to_string()))?;
        for frame in frames {
            let buffer = DynamicImage::ImageRgba8(frame.buffer().clone());
            let resized = fit_inside(&buffer, max).to_rgba8();
            size = (resized.width(), resized.height());
            encoder
                .encode_frame(Frame::from_parts(resized, 0, 0, frame.delay()))
                .map_err(|err| ImageError::Decode(err.to_string()))?;
        }
    }
    Ok((out, size.0, size.1))
}

/// A picture made ready to text (the CRM's `processSmsImage`).
///
/// Upright, at most 1600 px a side, re-encoded until it fits about 480 KB
/// (lower JPEG quality first, then smaller). PNG and GIF (animations too) keep
/// their format; HEIC and WebP become JPEG.
pub fn process_sms_image(bytes: &[u8]) -> Result<SmsImage, ImageError> {
    let kind = sniff(bytes).ok_or(ImageError::Unsupported)?;
    if kind == Kind::Gif {
        let frames = decode_gif_frames(bytes)?;
        let mut max = SMS_IMAGE_MAX_DIMENSION;
        let (mut out, mut width, mut height) = encode_gif(&frames, max)?;
        let mut guard = 0;
        while out.len() > SMS_IMAGE_TARGET_BYTES && max > 320 && guard < 6 {
            max = scale_down(max);
            (out, width, height) = encode_gif(&frames, max)?;
            guard += 1;
        }
        return Ok(SmsImage {
            bytes: out,
            content_type: "image/gif",
            ext: "gif",
            width,
            height,
        });
    }
    let image = decode(bytes)?;
    let png = kind == Kind::Png;
    let encode_at = |max: u32, quality: u8| {
        let resized = fit_inside(&image, max);
        let bytes = if png {
            encode_png(&resized)
        } else {
            encode_jpeg(&resized, quality)
        };
        (bytes, resized.width(), resized.height())
    };
    let mut max = SMS_IMAGE_MAX_DIMENSION;
    let mut quality: u8 = 82;
    let (mut out, mut width, mut height) = encode_at(max, quality);
    // PNG is lossless here: only a smaller size helps.
    while !png && out.len() > SMS_IMAGE_TARGET_BYTES && quality > 40 {
        quality -= 12;
        (out, width, height) = encode_at(max, quality);
    }
    let mut guard = 0;
    while out.len() > SMS_IMAGE_TARGET_BYTES && max > 320 && guard < 6 {
        max = scale_down(max);
        (out, width, height) = encode_at(max, quality);
        guard += 1;
    }
    Ok(SmsImage {
        bytes: out,
        content_type: if png { "image/png" } else { "image/jpeg" },
        ext: if png { "png" } else { "jpg" },
        width,
        height,
    })
}

/// 80 %, rounded.
const fn scale_down(max: u32) -> u32 {
    (max * 4 + 2) / 5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            let byte = |v: u32| u8::try_from(v % 256).expect("a byte");
            image::Rgb([byte(x), byte(y), byte(x * y)])
        }));
        encode_png(&image)
    }

    #[test]
    fn signatures_name_the_format() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Kind::Jpeg));
        assert_eq!(sniff(&png(2, 2)), Some(Kind::Png));
        assert_eq!(sniff(b"GIF89a...."), Some(Kind::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Kind::WebP));
        assert_eq!(
            sniff(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"),
            Some(Kind::Heic)
        );
        assert_eq!(
            sniff(b"\0\0\0\x18ftypmif1\0\0\0\0mif1heic"),
            Some(Kind::Heic)
        );
        assert_eq!(
            sniff(b"\0\0\0\x18ftypavif\0\0\0\0avifmif1"),
            Some(Kind::Avif)
        );
        assert_eq!(sniff(b"%PDF-1.7"), None);
    }

    #[test]
    fn big_pictures_shrink_to_the_text_size() {
        let processed = process_sms_image(&png(3000, 2000)).unwrap();
        assert_eq!(processed.content_type, "image/png");
        assert!(processed.width <= 1600 && processed.height <= 1600);
        assert!((i64::from(processed.width) * 2 - i64::from(processed.height) * 3).abs() <= 3);
        assert!(processed.bytes.len() <= SMS_IMAGE_TARGET_BYTES || processed.width <= 320);
        let jpeg = encode_jpeg(&decode(&png(3000, 2000)).unwrap(), 95);
        let processed = process_sms_image(&jpeg).unwrap();
        assert_eq!(
            (processed.content_type, processed.width, processed.height),
            ("image/jpeg", 1600, 1067)
        );
        assert!(processed.bytes.len() <= SMS_IMAGE_TARGET_BYTES);
    }

    #[test]
    fn small_pictures_keep_their_size() {
        let processed = process_sms_image(&png(40, 30)).unwrap();
        assert_eq!((processed.width, processed.height), (40, 30));
        assert!(matches!(
            process_sms_image(b"not an image"),
            Err(ImageError::Unsupported)
        ));
    }

    #[test]
    fn thumbnails_fit_320() {
        let thumb = thumbnail_jpeg(&png(1000, 500)).unwrap();
        let image = image::load_from_memory(&thumb).unwrap();
        assert_eq!((image.width(), image.height()), (320, 160));
    }

    #[test]
    fn transparency_turns_white_in_jpeg() {
        let clear = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0])));
        let jpeg = image::load_from_memory(&encode_jpeg(&clear, 90))
            .unwrap()
            .to_rgb8();
        assert!(jpeg.pixels().all(|p| p.0.iter().all(|c| *c > 245)));
    }
}
