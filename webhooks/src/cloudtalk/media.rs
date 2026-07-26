//! Inbound media engine (shape-agnostic pre-build).
//!
//! Given a media URL (source TBD — webhook field or fetch-by-id API), safely
//! fetch -> validate -> re-encode -> store to S3 -> insert
//! `cloudtalk_sms_attachments` rows. This module is payload-independent: it
//! makes no assumption about how the URL arrives. The thin wiring layer
//! (calling this from the `CloudTalk` webhook) is a separate, later task.
//!
//! Dedup contract (enforced by the future wiring layer, not by this module):
//! callers MUST run media processing only when the parent SMS insert actually
//! created a new row (`INSERT IGNORE` `rows_affected` > 0), so webhook
//! redelivery cannot duplicate attachment rows.
//!
//! v1 limitation: GIF animation is NOT preserved. Only the first frame is
//! decoded and re-encoded (matches `image`'s default single-frame decode).

use crate::amazon::bucket::S3Bucket;
use bytes::Bytes;
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use uuid::Uuid;

/// Short, non-sensitive error tokens. Never carries a URL, host, phone
/// number, or credential — only these fixed tokens are ever logged.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    #[error("fetch_disabled")]
    FetchDisabled,
    #[error("bad_scheme")]
    BadScheme,
    #[error("host_not_allowed")]
    HostNotAllowed,
    #[error("private_ip")]
    PrivateIp,
    #[error("too_large")]
    TooLarge,
    #[error("http_{0}")]
    Http(u16),
    #[error("timeout")]
    Timeout,
    #[error("network")]
    Network,
    #[error("unsupported_format")]
    UnsupportedFormat,
    #[error("decode_failed")]
    DecodeFailed,
    #[error("upload_failed")]
    Upload,
    #[error("db_failed")]
    Db,
}

// ===================== Component A — SSRF-hardened fetch =====================

/// Pure (no env, no network): validate scheme + host allowlist membership.
/// Factored out of `fetch_inbound_media` so it is unit-testable without a
/// connection. `allowed_hosts_raw` is the raw `MMS_MEDIA_ALLOWED_HOSTS` value
/// (comma-separated, case-insensitive exact host match). Returns the
/// lowercased host on success.
fn check_scheme_and_host(
    url: &reqwest::Url,
    allowed_hosts_raw: &str,
) -> Result<String, MediaError> {
    if url.scheme() != "https" {
        return Err(MediaError::BadScheme);
    }
    let host = url
        .host_str()
        .ok_or(MediaError::BadScheme)?
        .to_ascii_lowercase();

    if allowed_hosts_raw.trim().is_empty() {
        return Err(MediaError::FetchDisabled);
    }

    let allowed = allowed_hosts_raw
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .any(|h| h == host);

    if allowed {
        Ok(host)
    } else {
        Err(MediaError::HostNotAllowed)
    }
}

/// Pure: true if `ip` is safe to connect to (globally routable, not
/// loopback/private/link-local/unspecified/broadcast/documentation/etc).
/// Manual range checks are used where std has no stable helper (IPv6
/// unique-local `fc00::/7` and link-local `fe80::/10` are nightly-only in std).
const fn is_globally_routable(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => is_v4_global(v4),
        IpAddr::V6(v6) => is_v6_global(v6),
    }
}

const fn is_v4_global(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let cgnat = octets[0] == 100 && (octets[1] & 0xc0) == 64; // 100.64.0.0/10 (RFC 6598 CGNAT)
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || cgnat)
}

const fn is_v6_global(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let seg0 = ip.segments()[0];
    let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
    let unique_local = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
    !(link_local || unique_local)
}

/// Hard cap enforced both on `Content-Length` and on the running total while
/// streaming (never trust `Content-Length` alone).
const MAX_MEDIA_BYTES: u64 = 10 * 1024 * 1024;

/// Fetch inbound media bytes, SSRF-hardened. Fetch is OFF by default: unset
/// or empty `MMS_MEDIA_ALLOWED_HOSTS` always yields `FetchDisabled`.
pub async fn fetch_inbound_media(url: &str) -> Result<Bytes, MediaError> {
    fetch_inbound_media_inner(url)
        .await
        .inspect_err(|error| tracing::warn!(%error, "inbound media fetch failed"))
}

async fn fetch_inbound_media_inner(url: &str) -> Result<Bytes, MediaError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| MediaError::BadScheme)?;
    let allowed_hosts_raw = std::env::var("MMS_MEDIA_ALLOWED_HOSTS").unwrap_or_default();
    let host = check_scheme_and_host(&parsed, &allowed_hosts_raw)?;

    // Resolve DNS ourselves; every resolved address must be public, or we
    // reject outright (fail closed rather than pick a "good" address among bad ones).
    let resolved = tokio::net::lookup_host((host.as_str(), 443))
        .await
        .map_err(|_| MediaError::Network)?;

    let mut pinned_ip = None;
    for addr in resolved {
        if !is_globally_routable(addr.ip()) {
            return Err(MediaError::PrivateIp);
        }
        if pinned_ip.is_none() {
            pinned_ip = Some(addr.ip());
        }
    }
    let ip = pinned_ip.ok_or(MediaError::Network)?;

    // Pin the connection to the validated IP so reqwest cannot re-resolve
    // elsewhere between our check and the actual connect (TOCTOU).
    let client = reqwest::Client::builder()
        .resolve(&host, std::net::SocketAddr::new(ip, 443))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|_| MediaError::Network)?;

    let mut response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| map_reqwest_error(&error))?;

    if response.status().as_u16() != 200 {
        return Err(MediaError::Http(response.status().as_u16()));
    }

    if let Some(len) = response.content_length()
        && len > MAX_MEDIA_BYTES
    {
        return Err(MediaError::TooLarge);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_reqwest_error(&error))?
    {
        if body.len().saturating_add(chunk.len()) as u64 > MAX_MEDIA_BYTES {
            return Err(MediaError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Bytes::from(body))
}

fn map_reqwest_error(error: &reqwest::Error) -> MediaError {
    if error.is_timeout() {
        MediaError::Timeout
    } else {
        MediaError::Network
    }
}

// ===================== Component B — validate + re-encode =====================

/// Decode-bomb guard: reject before allocating pixel buffers for anything
/// declaring dimensions above this on either axis.
const MAX_DECODE_DIM: u32 = 8192;
/// Downscale target: longest side is capped here on output, aspect preserved.
const MAX_OUTPUT_DIM: u32 = 4096;
/// Fixed re-encode quality for the JPEG fallback path.
const JPEG_QUALITY: u8 = 80;
/// Decode allocation ceiling, set explicitly rather than inherited from
/// `image::Limits::default()` — an upstream default change must not silently
/// move our resource ceiling. Covers one worst-case RGBA8 buffer at
/// `MAX_DECODE_DIM` (8192*8192*4 = 256 MiB) plus decoder working headroom.
const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

/// A validated, re-encoded inbound image ready to store.
#[derive(Debug)]
pub struct ProcessedImage {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Real-type detection by magic bytes only — never trust a declared/claimed
/// type. Anything outside these four signatures is `unsupported_format`.
fn sniff_format(bytes: &[u8]) -> Option<image::ImageFormat> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(image::ImageFormat::Jpeg)
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(image::ImageFormat::Png)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(image::ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(image::ImageFormat::WebP)
    } else {
        None
    }
}

/// Validate, decode, and re-encode an inbound image.
///
/// PNG-with-alpha stays PNG; everything else (JPEG/WebP/GIF first
/// frame/opaque PNG) becomes JPEG at quality ~80. Downscales to a max of
/// 4096px on the longest side, aspect preserved; never upscales.
pub fn process_inbound_image(input: &[u8]) -> Result<ProcessedImage, MediaError> {
    let format = sniff_format(input).ok_or(MediaError::UnsupportedFormat)?;

    let mut reader = image::ImageReader::new(std::io::Cursor::new(input));
    reader.set_format(format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);

    let decoded = reader.decode().map_err(|_| MediaError::DecodeFailed)?;
    let keep_as_png = format == image::ImageFormat::Png && decoded.color().has_alpha();

    let (orig_w, orig_h) = (decoded.width(), decoded.height());
    let final_image = if orig_w.max(orig_h) > MAX_OUTPUT_DIM {
        decoded.resize(
            MAX_OUTPUT_DIM,
            MAX_OUTPUT_DIM,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        decoded
    };

    // Encode failures below fold into DecodeFailed: the locked error vocabulary has
    // no separate "encode" token, and we only ever encode images we just decoded.
    let mut out = Vec::new();
    let (content_type, ext) = if keep_as_png {
        final_image
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|_| MediaError::DecodeFailed)?;
        ("image/png", "png")
    } else {
        let rgb = final_image.to_rgb8();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|_| MediaError::DecodeFailed)?;
        ("image/jpeg", "jpg")
    };

    Ok(ProcessedImage {
        width: final_image.width(),
        height: final_image.height(),
        bytes: Bytes::from(out),
        content_type,
        ext,
    })
}

// ===================== Component C — store =====================

/// Upload a processed image to S3 and insert its `cloudtalk_sms_attachments` row.
///
/// Callers must only invoke this when the parent SMS insert actually created
/// a new row (see the dedup contract in the module docs) — this function
/// itself performs no dedup check.
pub async fn store_inbound_attachment(
    pool: &MySqlPool,
    s3: &impl S3Bucket,
    company_id: i32,
    sms_id: i32,
    img: &ProcessedImage,
    position: i32,
) -> Result<(), MediaError> {
    let bucket = std::env::var("STORAGE_BUCKET").map_err(|_| MediaError::Upload)?;
    let region = std::env::var("STORAGE_REGION").map_err(|_| MediaError::Upload)?;

    let file_uuid = Uuid::new_v4();
    let ext = img.ext;
    let key = format!("sms-attachments/{company_id}/inbound/{file_uuid}.{ext}");
    let filename = format!("inbound-{file_uuid}.{ext}");

    s3.send_file(&bucket, &key, img.bytes.clone())
        .await
        .map_err(|_| MediaError::Upload)?;

    // Public URL format is the CRM's canonical form for stored keys —
    // general_datebase/app/utils/s3.server.ts:51 is the authority.
    let s3_url = format!("https://{bucket}.s3.{region}.amazonaws.com/{key}");

    let width = i32::try_from(img.width).unwrap_or(i32::MAX);
    let height = i32::try_from(img.height).unwrap_or(i32::MAX);

    sqlx::query!(
        "INSERT INTO cloudtalk_sms_attachments \
            (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        sms_id,
        img.content_type,
        filename,
        key,
        s3_url,
        width,
        height,
        position,
    )
    .execute(pool)
    .await
    .map_err(|_| MediaError::Db)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod scheme_and_host {
        use super::*;

        fn url(s: &str) -> reqwest::Url {
            reqwest::Url::parse(s).expect("test url must parse")
        }

        #[test]
        fn rejects_http_scheme() {
            let result =
                check_scheme_and_host(&url("http://good.example.com/a.jpg"), "good.example.com");
            assert_eq!(result, Err(MediaError::BadScheme));
        }

        #[test]
        fn rejects_unlisted_host() {
            let result =
                check_scheme_and_host(&url("https://evil.example.com/a.jpg"), "good.example.com");
            assert_eq!(result, Err(MediaError::HostNotAllowed));
        }

        #[test]
        fn rejects_when_allowlist_empty() {
            let result = check_scheme_and_host(&url("https://good.example.com/a.jpg"), "");
            assert_eq!(result, Err(MediaError::FetchDisabled));
        }

        #[test]
        fn rejects_when_allowlist_unset_is_passed_as_empty_string() {
            // Caller reads env::var(...).unwrap_or_default(); unset becomes "".
            let result = check_scheme_and_host(&url("https://good.example.com/a.jpg"), "   ");
            assert_eq!(result, Err(MediaError::FetchDisabled));
        }

        #[test]
        fn accepts_allowed_https_host_case_insensitively() {
            // Stops at the DNS/pin boundary: only scheme + allowlist are checked here.
            let result =
                check_scheme_and_host(&url("https://Good.Example.COM/a.jpg"), "good.example.com");
            assert_eq!(result, Ok("good.example.com".to_string()));
        }

        #[test]
        fn accepts_host_from_multi_entry_allowlist() {
            let result = check_scheme_and_host(
                &url("https://b.example.com/a.jpg"),
                "a.example.com, b.example.com ,c.example.com",
            );
            assert_eq!(result, Ok("b.example.com".to_string()));
        }
    }

    mod public_ip {
        use super::*;

        #[test]
        fn rejects_v4_loopback() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }

        #[test]
        fn rejects_v4_private_10() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                10, 0, 0, 1
            ))));
        }

        #[test]
        fn rejects_v4_private_172_16() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                172, 16, 0, 1
            ))));
        }

        #[test]
        fn rejects_v4_private_192_168() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                192, 168, 1, 1
            ))));
        }

        #[test]
        fn rejects_v4_link_local_cloud_metadata() {
            // 169.254.169.254 — AWS/GCP instance metadata endpoint.
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                169, 254, 169, 254
            ))));
        }

        #[test]
        fn rejects_v6_loopback() {
            assert!(!is_globally_routable(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        }

        #[test]
        fn rejects_v6_link_local() {
            let ip: Ipv6Addr = "fe80::1".parse().unwrap();
            assert!(!is_globally_routable(IpAddr::V6(ip)));
        }

        #[test]
        fn rejects_v6_unique_local() {
            let ip: Ipv6Addr = "fc00::1".parse().unwrap();
            assert!(!is_globally_routable(IpAddr::V6(ip)));
        }

        #[test]
        fn rejects_v4_mapped_private_v6() {
            // ::ffff:10.0.0.1 — v4-mapped; must re-check the embedded v4 against v4 rules.
            let ip: Ipv6Addr = "::ffff:10.0.0.1".parse().unwrap();
            assert!(!is_globally_routable(IpAddr::V6(ip)));
        }

        #[test]
        fn accepts_public_v4() {
            assert!(is_globally_routable(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        }

        #[test]
        fn accepts_public_v6() {
            let ip: Ipv6Addr = "2001:4860:4860::8888".parse().unwrap();
            assert!(is_globally_routable(IpAddr::V6(ip)));
        }

        #[test]
        fn rejects_v4_unspecified() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        }

        #[test]
        fn rejects_v4_broadcast() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::BROADCAST)));
        }

        #[test]
        fn rejects_v4_cgnat() {
            // 100.64.0.1 — inside 100.64.0.0/10 (RFC 6598 carrier-grade NAT).
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                100, 64, 0, 1
            ))));
        }

        #[test]
        fn accepts_v4_just_below_cgnat_range() {
            // 100.63.255.255 — one address below the CGNAT block.
            assert!(is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                100, 63, 255, 255
            ))));
        }

        #[test]
        fn accepts_v4_just_above_cgnat_range() {
            // 100.128.0.0 — one address above the CGNAT block.
            assert!(is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                100, 128, 0, 0
            ))));
        }

        #[test]
        fn rejects_v4_multicast() {
            assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(
                224, 0, 0, 1
            ))));
        }

        #[test]
        fn rejects_v6_multicast() {
            // ff02::1 — link-local "all nodes" multicast.
            let ip: Ipv6Addr = "ff02::1".parse().unwrap();
            assert!(!is_globally_routable(IpAddr::V6(ip)));
        }
    }

    mod process_image {
        use super::*;

        // All fixtures below are real, valid, `image`-crate-encoded bytes built at
        // test runtime — no binary files are checked into the repository. This means
        // every fixture is a same-library round-trip (encoded and decoded by the same
        // `image` crate our production code uses), unlike the sips/ffmpeg-generated
        // files this replaced, which were cross-encoder (a different, independent
        // encoder produced the bytes our code then decoded). That cross-encoder
        // property is not reproducible without checking in external binaries, so it
        // is not claimed here — see the report's "Fixture rework" section.

        /// A real, validly-encoded opaque PNG (no alpha channel at all: `ColorType::Rgb8`).
        fn synthetic_png(width: u32, height: u32) -> Vec<u8> {
            let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .expect("synthetic PNG must encode");
            out
        }

        /// A real PNG with an alpha channel that is actually used: one pixel is
        /// genuinely non-opaque, not just format-tagged as RGBA.
        fn synthetic_alpha_png(width: u32, height: u32) -> Vec<u8> {
            let mut img =
                image::RgbaImage::from_pixel(width, height, image::Rgba([200, 40, 40, 255]));
            img.put_pixel(0, 0, image::Rgba([200, 40, 40, 128]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .expect("synthetic alpha PNG must encode");
            out
        }

        /// A real, validly-encoded JPEG.
        fn synthetic_jpeg(width: u32, height: u32) -> Vec<u8> {
            let img = image::RgbImage::from_pixel(width, height, image::Rgb([180, 90, 40]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut out),
                    image::ImageFormat::Jpeg,
                )
                .expect("synthetic JPEG must encode");
            out
        }

        /// A real, single-frame GIF via the `image` crate's own GIF encoder.
        fn synthetic_gif(width: u32, height: u32, color: image::Rgba<u8>) -> Vec<u8> {
            let mut out = Vec::new();
            {
                let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
                let frame = image::RgbaImage::from_pixel(width, height, color);
                encoder
                    .encode_frame(image::Frame::new(frame))
                    .expect("synthetic GIF frame must encode");
            }
            out
        }

        /// A real, multi-frame animated GIF: frame 0 red, frame 1 blue — same visual
        /// design as the fixture this replaces, so the first-frame discrimination
        /// test below is unchanged.
        fn synthetic_animated_gif(width: u32, height: u32) -> Vec<u8> {
            let mut out = Vec::new();
            {
                let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
                let red =
                    image::RgbaImage::from_pixel(width, height, image::Rgba([220, 20, 20, 255]));
                let blue =
                    image::RgbaImage::from_pixel(width, height, image::Rgba([20, 20, 220, 255]));
                encoder
                    .encode_frame(image::Frame::new(red))
                    .expect("synthetic GIF frame 0 must encode");
                encoder
                    .encode_frame(image::Frame::new(blue))
                    .expect("synthetic GIF frame 1 must encode");
            }
            out
        }

        /// A real generated JPEG, truncated well before any entropy-coded scan data:
        /// keeps the SOI/JFIF magic bytes (so sniffing still says "JPEG") but cannot
        /// decode — same failure mode the old truncated-sips-file fixture proved.
        fn synthetic_corrupt_jpeg() -> Vec<u8> {
            let full = synthetic_jpeg(16, 16);
            let cut = 20.min(full.len());
            full[..cut].to_vec()
        }

        #[test]
        fn jpeg_input_reencodes_as_jpeg() {
            let input = synthetic_jpeg(16, 16);
            let out = process_inbound_image(&input).expect("synthetic jpeg must process");
            assert_eq!(out.content_type, "image/jpeg");
            assert_eq!(out.ext, "jpg");
            assert_eq!((out.width, out.height), (16, 16));
        }

        #[test]
        fn opaque_png_reencodes_as_jpeg() {
            let input = synthetic_png(16, 16);
            let out = process_inbound_image(&input).expect("synthetic opaque png must process");
            assert_eq!(
                out.content_type, "image/jpeg",
                "opaque PNG must re-encode to JPEG, not stay PNG"
            );
            assert_eq!(out.ext, "jpg");
        }

        #[test]
        fn alpha_png_stays_png() {
            let input = synthetic_alpha_png(16, 16);
            let out = process_inbound_image(&input).expect("synthetic alpha png must process");
            assert_eq!(out.content_type, "image/png");
            assert_eq!(out.ext, "png");
            assert_eq!((out.width, out.height), (16, 16));
        }

        #[test]
        fn gif_reencodes_as_jpeg() {
            let input = synthetic_gif(16, 16, image::Rgba([40, 160, 60, 255]));
            let out = process_inbound_image(&input).expect("synthetic gif must process");
            assert_eq!(out.content_type, "image/jpeg");
        }

        #[test]
        fn animated_gif_keeps_only_first_frame() {
            let input = synthetic_animated_gif(16, 16);
            let out = process_inbound_image(&input).expect("synthetic animated gif must process");

            let decoded = image::load_from_memory_with_format(&out.bytes, image::ImageFormat::Jpeg)
                .expect("re-encoded output must itself decode")
                .to_rgb8();
            let pixel = decoded.get_pixel(0, 0);
            assert!(
                pixel.0[0] > pixel.0[2],
                "expected first-frame red to dominate over second-frame blue, got {:?}",
                pixel.0
            );
        }

        #[test]
        fn corrupt_bytes_with_valid_magic_fails_decode() {
            let input = synthetic_corrupt_jpeg();
            assert_eq!(
                process_inbound_image(&input).unwrap_err(),
                MediaError::DecodeFailed
            );
        }

        #[test]
        fn bytes_with_no_recognized_magic_are_unsupported() {
            let input = b"this is plain text, not an image at all";
            assert_eq!(
                process_inbound_image(input).unwrap_err(),
                MediaError::UnsupportedFormat
            );
        }

        #[test]
        fn sniff_format_recognizes_webp_container_header() {
            // RIFF....WEBP container header — the public, documented magic-number
            // layout of the format, not an invented payload. No local WebP encoder
            // is available in this environment, so this is the only WebP coverage;
            // see the report for provenance.
            let mut header = Vec::from(*b"RIFF");
            header.extend_from_slice(&[0, 0, 0, 0]); // chunk size, irrelevant to sniffing
            header.extend_from_slice(b"WEBP");
            assert_eq!(sniff_format(&header), Some(image::ImageFormat::WebP));
        }

        #[test]
        fn oversized_dimension_is_rejected_by_decode_limit() {
            let input = synthetic_png(MAX_DECODE_DIM + 1, 1);
            assert_eq!(
                process_inbound_image(&input).unwrap_err(),
                MediaError::DecodeFailed
            );
        }

        #[test]
        fn dimension_exactly_at_cap_is_accepted() {
            let input = synthetic_png(MAX_DECODE_DIM, 1);
            let out = process_inbound_image(&input)
                .expect("dimension exactly at the decode cap must be accepted");
            assert!(
                out.width.max(out.height) <= MAX_OUTPUT_DIM,
                "must also have gone through the downscale path: got {}x{}",
                out.width,
                out.height
            );
        }

        #[test]
        fn oversized_but_under_cap_image_is_downscaled_to_4096() {
            let input = synthetic_png(4200, 2100);
            let out =
                process_inbound_image(&input).expect("under-cap synthetic image must process");
            assert!(out.width <= MAX_OUTPUT_DIM && out.height <= MAX_OUTPUT_DIM);
            assert!(
                out.width < 4200 && out.height < 2100,
                "must actually downscale, not just cap-check: got {}x{}",
                out.width,
                out.height
            );
            let in_ratio = 4200f64 / 2100f64;
            let out_ratio = f64::from(out.width) / f64::from(out.height);
            assert!(
                (in_ratio - out_ratio).abs() < 0.02,
                "aspect ratio must be preserved, got {out_ratio}"
            );
        }
    }

    mod store {
        use super::*;
        use crate::tests::utils::MockClient;

        /// Same fixed values on every call — safe under parallel test execution
        /// since no other test in this crate reads/writes these two env vars.
        fn set_storage_env() {
            unsafe {
                std::env::set_var("STORAGE_BUCKET", "test-bucket");
                std::env::set_var("STORAGE_REGION", "us-east-2");
            }
        }

        async fn insert_parent_sms(pool: &MySqlPool, company_id: i32) -> i32 {
            let rec = sqlx::query!(
                "INSERT INTO cloudtalk_sms \
                    (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
                 VALUES (NULL, NULL, 3173161456, 'mms', '540273', ?, 'inbound', 'received')",
                company_id,
            )
            .execute(pool)
            .await
            .unwrap();
            i32::try_from(rec.last_insert_id()).expect("test id fits i32")
        }

        fn sample_image(width: u32, height: u32) -> ProcessedImage {
            ProcessedImage {
                bytes: Bytes::from_static(b"fake-jpeg-bytes"),
                content_type: "image/jpeg",
                ext: "jpg",
                width,
                height,
            }
        }

        #[sqlx::test(migrations = "../migrations")]
        async fn store_inserts_row_with_expected_key_and_url_format(pool: MySqlPool) {
            set_storage_env();
            let sms_id = insert_parent_sms(&pool, 42).await;
            let s3 = MockClient::new("unused");
            let img = sample_image(800, 600);

            store_inbound_attachment(&pool, &s3, 42, sms_id, &img, 0)
                .await
                .expect("store must succeed");

            let row = sqlx::query!(
                "SELECT cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height, position \
                 FROM cloudtalk_sms_attachments WHERE cloudtalk_sms_id = ?",
                sms_id,
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(row.cloudtalk_sms_id, sms_id);
            assert_eq!(row.content_type, "image/jpeg");
            assert_eq!(row.width, Some(800));
            assert_eq!(row.height, Some(600));
            assert_eq!(row.position, 0);

            let prefix = "sms-attachments/42/inbound/";
            assert!(row.s3_key.starts_with(prefix), "key was {}", row.s3_key);
            let has_jpg_ext = std::path::Path::new(&row.s3_key)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jpg"));
            assert!(has_jpg_ext, "key was {}", row.s3_key);
            let uuid_part = &row.s3_key[prefix.len()..row.s3_key.len() - 4];
            assert!(
                uuid::Uuid::parse_str(uuid_part).is_ok(),
                "expected a uuid segment in key, got {uuid_part}"
            );

            assert_eq!(row.filename, format!("inbound-{uuid_part}.jpg"));
            assert_eq!(
                row.s3_url,
                format!(
                    "https://test-bucket.s3.us-east-2.amazonaws.com/{}",
                    row.s3_key
                ),
                "must match the CRM's public URL format exactly"
            );
        }

        #[sqlx::test(migrations = "../migrations")]
        async fn store_orders_two_attachments_by_position(pool: MySqlPool) {
            set_storage_env();
            let sms_id = insert_parent_sms(&pool, 42).await;
            let s3 = MockClient::new("unused");

            store_inbound_attachment(&pool, &s3, 42, sms_id, &sample_image(100, 100), 0)
                .await
                .unwrap();
            store_inbound_attachment(&pool, &s3, 42, sms_id, &sample_image(200, 200), 1)
                .await
                .unwrap();

            let rows = sqlx::query!(
                "SELECT width, position FROM cloudtalk_sms_attachments \
                 WHERE cloudtalk_sms_id = ? ORDER BY position",
                sms_id,
            )
            .fetch_all(&pool)
            .await
            .unwrap();

            assert_eq!(rows.len(), 2);
            assert_eq!((rows[0].position, rows[0].width), (0, Some(100)));
            assert_eq!((rows[1].position, rows[1].width), (1, Some(200)));
        }
    }
}
