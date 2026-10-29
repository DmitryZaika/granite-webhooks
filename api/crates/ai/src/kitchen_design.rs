//! AI Design: a customer's kitchen photo with a stone installed.
//!
//! Port of the CRM's `api.ai-design` action: the photo and the stone's slab
//! picture are prepared, the slab is described by the chat model, and the
//! image model redraws the kitchen on the chosen surfaces (one picture per
//! stone).

use crate::ai::{Caller, EditImage, IMAGE_MODEL, MINI_MODEL};
use crate::images::{self, encode_png, fit_inside};
use crate::state::AppState;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::{DynamicImage, imageops::FilterType};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;

/// Stones one request may compare.
pub const MAX_STONES: usize = 3;
/// Biggest kitchen photo taken: as base64 in the JSON body it stays under
/// the 6 MB a Lambda request may have ([`MAX_DESIGN_BODY_BYTES`]).
pub const MAX_KITCHEN_BYTES: usize = 4 * 1024 * 1024;
/// Biggest `POST /v1/ai/design` body (axum takes 2 MB unless told).
pub const MAX_DESIGN_BODY_BYTES: usize = 6 * 1024 * 1024;
const MAX_REMOTE_BYTES: usize = 30 * 1024 * 1024;

/// Where the stone goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Countertops,
    Fireplace,
    FullHeightBacksplash,
}

impl Surface {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "countertops" => Some(Self::Countertops),
            "fireplace" => Some(Self::Fireplace),
            "full_height_backsplash" => Some(Self::FullHeightBacksplash),
            _ => None,
        }
    }
}

/// `parseSurfaces`: the known ones, once each; countertops when none.
pub fn parse_surfaces(values: &[String]) -> Vec<Surface> {
    let mut out = Vec::new();
    for value in values {
        if let Some(surface) = Surface::parse(value.trim())
            && !out.contains(&surface)
        {
            out.push(surface);
        }
    }
    if out.is_empty() {
        out.push(Surface::Countertops);
    }
    out
}

/// What the prompt says about the references it got.
#[derive(Debug, Clone, Default)]
pub struct References {
    pub swatch: bool,
    pub installed: bool,
    pub stone_profile: String,
}

/// `buildDesignPrompt`.
#[allow(clippy::too_many_lines)]
pub fn build_prompt(
    stone_name: &str,
    stone_type: Option<&str>,
    surfaces: &[Surface],
    extra: &str,
    refs: &References,
) -> String {
    let mut apply = Vec::new();
    if surfaces.contains(&Surface::Countertops) {
        apply.push("countertop horizontal surfaces (island top + perimeter counter tops)");
    }
    if surfaces.contains(&Surface::Fireplace) {
        apply.push("fireplace surround and hearth");
    }
    if surfaces.contains(&Surface::FullHeightBacksplash) {
        apply.push("full-height backsplash between counters and upper cabinets");
    }
    let extra = extra.trim();
    let backsplash_locked = !surfaces.contains(&Surface::FullHeightBacksplash)
        && !crate::re!(r"\bbacksplash\b", "i").is_match(extra);
    let has_changes = !extra.is_empty();
    let profile = refs.stone_profile.trim();
    let material = stone_type
        .filter(|t| !t.is_empty())
        .map_or_else(|| stone_name.to_string(), |t| format!("{stone_name} ({t})"));
    let mut reference_lines = vec![
        "Image 1 = the original kitchen photo. Preserve this room exactly unless the user explicitly asks to change something below."
            .to_string(),
    ];
    if refs.swatch {
        reference_lines.push(
            "Image 2 = inventory slab photo. Exact material color, undertone, and veining reference.".into(),
        );
    }
    if refs.installed {
        reference_lines.push(format!(
            "{} = real installed project showing this stone on counters. Match how the stone looks in this install photo.",
            if refs.swatch { "Image 3" } else { "Image 2" }
        ));
    }
    let mut prompt: Vec<String> = vec![
        "TASK: Real photograph with stone swapped on selected surfaces. Not a render, not stylized art.".into(),
        String::new(),
        "REFERENCE IMAGES:".into(),
    ];
    prompt.extend(reference_lines);
    prompt.push(if refs.swatch || refs.installed {
        "Reference images must NOT appear in the output.".into()
    } else {
        format!("Material: {material}. Match this stone as closely as possible.")
    });
    prompt.push(String::new());
    prompt.push("APPLY STONE TO THESE SURFACES ONLY:".into());
    prompt.extend(apply.iter().map(|part| format!("- {part}")));
    if !profile.is_empty() {
        prompt.extend([
            String::new(),
            "EXACT STONE APPEARANCE (from inventory slab analysis):".into(),
            profile.to_string(),
            "Applied stone must match these colors and patterns exactly. Do not simplify, grey out, beige out, or shift the palette.".into(),
        ]);
    }
    prompt.extend(
        [
            "",
            "STONE MATCHING (highest priority on allowed surfaces):",
            "- Copy exact base color, undertones, and secondary hues from the slab reference",
            "- Copy vein pattern, direction, density, scale, and contrast from the slab reference",
            "- If the slab has sage green fields, bright white patches, and charcoal black veins — reproduce those same colors on counters",
            "- Match polish level and natural stone texture from the reference",
            "- Result must look like real fabricated stone from this slab, as in a showroom or install photo",
            "",
            "SWATCH BACKGROUND — IGNORE COMPLETELY:",
            "- Reference photos may show the slab on a white, grey, or studio background — ignore that background entirely",
            "- Sample color, veining, and finish ONLY from stone slab pixels",
            "- Never copy the photo background onto countertops or anywhere in the output",
            "",
            "COLOR ACCURACY — NO FILTERS:",
            "- Output must look like an unedited photograph, not AI art",
            "- Do NOT apply cinematic grading, HDR, vignette, blur, glow, haze, or color filters",
            "- Do NOT lighten, darken, warm, cool, desaturate, or shift hues on the applied stone",
            "- Do NOT grey-out, beige-out, or mute the stone palette",
            "- Applied stone colors must match the slab reference exactly under the kitchen lighting",
            "- Keep Image 1 overall white balance, exposure, and color grading unchanged",
            "- Only selected surface material pixels change; every other color in the photo stays identical",
        ]
        .map(str::to_string),
    );
    if backsplash_locked {
        prompt.extend(
            [
                "",
                "BACKSPLASH AND WALLS BEHIND COUNTERS — DO NOT CHANGE:",
                "- Keep the backsplash, wall tiles, and vertical surfaces behind counters exactly as Image 1",
                "- Do NOT apply stone to the backsplash or any wall area unless listed in APPLY STONE TO above",
                "- Countertop material stops at the counter edge; nothing above the counter may change",
            ]
            .map(str::to_string),
        );
    }
    if has_changes {
        prompt.push(String::new());
        prompt.push("DEFAULT: Do not change cabinets, walls, floor, appliances, decor, or layout unless the user asks below.".into());
        if backsplash_locked {
            prompt.push(
                "- Backsplash stays unchanged unless the user explicitly asked to change it below"
                    .into(),
            );
        }
        prompt.extend([
            String::new(),
            "USER REQUESTED CHANGES — you MAY change other details ONLY as explicitly described here:".into(),
            extra.to_string(),
            String::new(),
            "Apply only what the user wrote above. Do not add extra redesigns they did not ask for.".into(),
        ]);
    } else {
        let mut locked = vec![
            "",
            "DO NOT CHANGE unless listed in APPLY STONE TO above:",
            "- Cabinets: same style, color, finish, doors, hardware — no new hoods or cabinet redesign",
        ];
        if !surfaces.contains(&Surface::Countertops) {
            locked.push("- Countertops — keep original material");
        }
        if !surfaces.contains(&Surface::Fireplace) {
            locked.push("- Fireplace — keep original material");
        }
        locked.extend([
            "- Walls, ceiling, trim, flooring, rugs, appliances, fixtures, windows, decor, and objects",
            "- Camera angle, lighting, and composition",
            "",
            "Do not add or remove objects. Do not restyle the room.",
        ]);
        // `.filter(line => line.length > 0)` in the CRM drops the blank lines.
        prompt.extend(
            locked
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(str::to_string),
        );
    }
    prompt.push(String::new());
    prompt.push(if has_changes {
        format!(
            "FINAL CHECK: stone only on selected surfaces{}, only user-requested extras applied.",
            if backsplash_locked {
                ", backsplash unchanged"
            } else {
                ""
            }
        )
    } else {
        format!(
            "FINAL CHECK: stone only on selected surfaces{}, everything else identical to Image 1.",
            if backsplash_locked {
                ", backsplash identical to Image 1"
            } else {
                ""
            }
        )
    });
    prompt.join("\n")
}

/// A prepared picture: PNG bytes.
pub struct Prepared {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn prepared(image: &DynamicImage) -> Prepared {
    Prepared {
        png: encode_png(image),
        width: image.width(),
        height: image.height(),
    }
}

/// `prepareKitchenImage`: upright, at most 2048 px, PNG.
pub fn prepare_kitchen(bytes: &[u8]) -> Result<Prepared, images::ImageError> {
    Ok(prepared(&fit_inside(&images::decode(bytes)?, 2048)))
}

/// sharp's `trim({ threshold })`: the edges that match the top-left pixel cut off.
fn trim_background(image: &DynamicImage, threshold: u8) -> Option<DynamicImage> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let background = *rgba.get_pixel(0, 0);
    let differs = |x: u32, y: u32| {
        let p = rgba.get_pixel(x, y);
        p.0.iter()
            .zip(background.0.iter())
            .any(|(a, b)| a.abs_diff(*b) > threshold)
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
    (left <= right && top <= bottom)
        .then(|| image.crop_imm(left, top, right - left + 1, bottom - top + 1))
}

/// `prepareSwatchImage`: upright, the studio background trimmed, small
/// slabs enlarged to 1024 px and big ones cut to 1536 px, PNG.
pub fn prepare_swatch(bytes: &[u8]) -> Result<Prepared, images::ImageError> {
    let image = images::decode(bytes)?;
    let image = trim_background(&image, 12).unwrap_or(image);
    let min_side = image.width().min(image.height());
    let image = if min_side > 0 && min_side < 1024 {
        image.resize(1024, 1024, FilterType::Lanczos3)
    } else if min_side > 1536 {
        fit_inside(&image, 1536)
    } else {
        image
    };
    Ok(prepared(&image))
}

/// `getOutputAspectRatio` → the image size asked for.
pub fn output_size(kitchen: &Prepared) -> &'static str {
    if kitchen.width == 0 || kitchen.height == 0 {
        return "1536x1024";
    }
    let ratio = f64::from(kitchen.width) / f64::from(kitchen.height);
    if ratio > 1.15 {
        "1536x1024"
    } else if ratio < 0.87 {
        "1024x1536"
    } else {
        "1024x1024"
    }
}

/// Only public web addresses are fetched.
fn fetchable(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => host != "localhost" && !host.ends_with(".internal"),
        Some(url::Host::Ipv4(ip)) => {
            let ip = IpAddr::V4(ip);
            !(ip.is_loopback()
                || ip.is_unspecified()
                || matches!(ip, IpAddr::V4(v4) if v4.is_private() || v4.is_link_local()))
        }
        Some(url::Host::Ipv6(ip)) => !(ip.is_loopback() || ip.is_unspecified()),
        None => false,
    }
}

/// `loadRemoteImage`: a stone picture (the full size first for an `-icon.`
/// one), from the bucket when it is ours, else over HTTP.
pub async fn load_remote(state: &AppState, url: &str) -> Option<Vec<u8>> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut candidates = vec![trimmed.to_string()];
    if trimmed.contains("-icon.") {
        candidates.insert(0, trimmed.replacen("-icon.", ".", 1));
    }
    for candidate in candidates {
        if let Some(key) = state.storage.key_for_url(&candidate)
            && let Ok(Some(object)) = state.storage.get(&key).await
        {
            return Some(object.bytes);
        }
        if !fetchable(&candidate) {
            continue;
        }
        let Ok(response) = state
            .telephony_http
            .get(&candidate)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        if let Ok(bytes) = response.bytes().await
            && bytes.len() <= MAX_REMOTE_BYTES
        {
            return Some(bytes.to_vec());
        }
    }
    None
}

/// A stone as AI Design uses it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DesignStone {
    pub id: i64,
    pub name: String,
    pub url: Option<String>,
    #[sqlx(rename = "type")]
    pub kind: Option<String>,
}

/// `loadInstalledStoneImage`: a photo of this stone (or the stone it borrows
/// pictures from) installed.
async fn installed_photo(state: &AppState, stone_id: i64) -> Option<Vec<u8>> {
    let direct: Option<String> =
        sqlx::query_scalar("SELECT url FROM installed_stones WHERE stone_id = ? LIMIT 1")
            .bind(stone_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    if let Some(url) = direct.filter(|u| !u.is_empty()) {
        return load_remote(state, &url).await;
    }
    let linked: Option<String> = sqlx::query_scalar(
        "SELECT is2.url FROM stone_image_links sil
           JOIN installed_stones is2 ON is2.stone_id = sil.source_stone_id
          WHERE sil.stone_id = ? LIMIT 1",
    )
    .bind(stone_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    match linked.filter(|u| !u.is_empty()) {
        Some(url) => load_remote(state, &url).await,
        None => None,
    }
}

fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", STANDARD.encode(png))
}

/// `analyzeStoneAppearance`: the slab described for the image model; empty
/// when the model fails.
async fn describe_stone(
    state: &AppState,
    caller: Caller<'_>,
    swatch: &Prepared,
    stone: &DesignStone,
) -> String {
    let material = stone
        .kind
        .as_deref()
        .filter(|t| !t.is_empty())
        .map_or_else(|| stone.name.clone(), |t| format!("{} ({t})", stone.name));
    let body = json!({
        "model": MINI_MODEL,
        "max_completion_tokens": 500,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": format!("Analyze this {material} slab photograph. Describe ONLY the stone material for matching on kitchen countertops. Be very specific about colors (for example sage green, olive grey, charcoal black, bright white). Cover base colors, undertones, primary veins, fine secondary veins, contrast, polish level, and overall pattern. Ignore any photo background or text overlay. Plain bullet points only, no markdown.") },
                { "type": "image_url", "image_url": { "url": data_url(&swatch.png), "detail": "high" } },
            ],
        }],
    });
    match state
        .ai
        .complete(
            &state.pool,
            caller,
            MINI_MODEL,
            &body,
            Duration::from_secs(120),
        )
        .await
    {
        Ok(message) => message["content"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        Err(err) => {
            lambda_http::tracing::warn!(error = %err, "AI design image description request failed");
            String::new()
        }
    }
}

/// One stone's design (`generateDesignImage`): the picture, or why not.
pub async fn design(
    state: &AppState,
    caller: Caller<'_>,
    kitchen: &Prepared,
    stone: &DesignStone,
    surfaces: &[Surface],
    extra: &str,
) -> Result<String, String> {
    let swatch_and_profile = async {
        let raw = match stone.url.as_deref() {
            Some(url) => load_remote(state, url).await,
            None => None,
        };
        let swatch = raw.and_then(|bytes| prepare_swatch(&bytes).ok());
        let profile = match &swatch {
            Some(swatch) => describe_stone(state, caller, swatch, stone).await,
            None => String::new(),
        };
        (swatch, profile)
    };
    let installed = async {
        installed_photo(state, stone.id)
            .await
            .and_then(|bytes| prepare_kitchen(&bytes).ok())
    };
    let ((swatch, profile), installed) = tokio::join!(swatch_and_profile, installed);
    let mut pictures = vec![EditImage {
        mime: "image/png".into(),
        bytes: kitchen.png.clone(),
    }];
    if let Some(swatch) = &swatch {
        pictures.push(EditImage {
            mime: "image/png".into(),
            bytes: swatch.png.clone(),
        });
    }
    if let Some(installed) = &installed {
        pictures.push(EditImage {
            mime: "image/png".into(),
            bytes: installed.png.clone(),
        });
    }
    let prompt = build_prompt(
        &stone.name,
        stone.kind.as_deref(),
        surfaces,
        extra,
        &References {
            swatch: swatch.is_some(),
            installed: installed.is_some(),
            stone_profile: profile,
        },
    );
    match state
        .ai
        .image_edit(
            &state.pool,
            caller,
            IMAGE_MODEL,
            &prompt,
            &pictures,
            output_size(kitchen),
        )
        .await
    {
        Ok(Some(image)) => Ok(image),
        Ok(None) => Err("Image generation returned no result".into()),
        Err(err) => Err(err.to_string()),
    }
}

/// The JSON line of a design event.
pub fn event(value: &Value) -> String {
    format!("{value}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_default_to_countertops() {
        assert_eq!(parse_surfaces(&[]), vec![Surface::Countertops]);
        assert_eq!(
            parse_surfaces(&["fireplace".into(), "bogus".into(), "fireplace".into()]),
            vec![Surface::Fireplace]
        );
    }

    #[test]
    fn a_white_border_is_trimmed() {
        let mut img = image::RgbaImage::from_pixel(10, 8, image::Rgba([255, 255, 255, 255]));
        for x in 2..7 {
            for y in 3..6 {
                img.put_pixel(x, y, image::Rgba([10, 120, 30, 255]));
            }
        }
        let trimmed = trim_background(&DynamicImage::ImageRgba8(img), 12).unwrap();
        assert_eq!((trimmed.width(), trimmed.height()), (5, 3));
        assert!(!fetchable("http://169.254.169.254/latest"));
        assert!(!fetchable("file:///etc/passwd"));
        assert!(fetchable("https://img.example/slab.jpg"));
    }
}
