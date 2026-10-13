//! Preparing a PDF to send for e-signature.
//!
//! Its form is frozen into the page (pdf-lib's `form.flatten()`), and what
//! the sender filled in is printed on it (text and dates in Helvetica,
//! their drawn signature as a picture). Ports of the CRM's `pdfTextStamp.ts`, `pdfSigningGeometry.ts`,
//! `pdfSigningFields.server.ts` (`preparePdfForSignWell`) and
//! `nativeSignaturePdf.server.ts` (`prepareNativeSignaturePdf`). Field boxes
//! are in top-left page points, as the signing editor shows the page.

// Positions follow pdf-lib's arithmetic exactly (no fused multiply-add), so
// stamps land on the same points.
#![allow(clippy::suboptimal_flops)]

use crate::pdf::{Font, Image};
use base64::Engine;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use unicode_normalization::UnicodeNormalization;
use utoipa::ToSchema;

pub const SIGNATURE_IMAGE_PREFIX: &str = "data:image/png;base64,";
const SIGNATURE_IMAGE_MAX_LENGTH: usize = 300_000;
const MAX_FONT_SIZE: f32 = 11.0;
const MIN_FONT_SIZE: f32 = 6.0;
const LINE_GAP: f32 = 1.15;
const BASELINE_PAD: f32 = 2.0;
const SIDE_PAD: f32 = 2.0;
/// pdf-lib's `rgb(0.06, 0.09, 0.16)`.
const TEXT_COLOR: &str = "0.06 0.09 0.16 rg";

/// A field on the document: one the signer fills in, or one the sender
/// filled (`value`), which is printed before it goes out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SignatureField {
    /// `signature`, `date`, `text` or `autofill_date_signed`.
    #[serde(rename = "type")]
    pub kind: FieldKind,
    /// From 1.
    pub page: i64,
    /// Left edge, points from the page's left.
    pub x: f64,
    /// Top edge, points from the page's top.
    pub y: f64,
    /// Points wide.
    pub width: f64,
    /// Points high.
    pub height: f64,
    /// The signer must fill it.
    pub required: bool,
    /// What the sender filled in (a signature: a PNG data URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Text fields: which customer detail filled the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
    /// The editor's name for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Native signing: the field's stable id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// What a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Signature,
    Date,
    Text,
    AutofillDateSigned,
}

const PREFILL_KEYS: [&str; 11] = [
    "customer_name",
    "company_name",
    "billing_address",
    "billing_address_line1",
    "billing_address_line2",
    "email",
    "phone",
    "sender_name",
    "sender_email",
    "today",
    "date",
];

impl SignatureField {
    fn trimmed_value(&self) -> &str {
        self.value.as_deref().map_or("", str::trim)
    }

    /// Printed by us (`isStampedField`); empty ones are the signer's.
    pub fn is_stamped(&self) -> bool {
        match self.kind {
            FieldKind::AutofillDateSigned => false,
            FieldKind::Signature => self.trimmed_value().starts_with(SIGNATURE_IMAGE_PREFIX),
            _ => !self.trimmed_value().is_empty(),
        }
    }

    /// The editor's limits (`signatureFieldConfigSchema`).
    pub fn check(&self) -> Result<(), &'static str> {
        let within =
            |value: f64, min: f64, max: f64| value.is_finite() && value >= min && value <= max;
        if !(1..=500).contains(&self.page)
            || !within(self.x, 0.0, 2000.0)
            || !within(self.y, 0.0, 4000.0)
            || !within(self.width, 20.0, 600.0)
            || !within(self.height, 12.0, 200.0)
        {
            return Err("Invalid signature field");
        }
        let value = self.value.as_deref().unwrap_or("");
        if value.chars().count() > SIGNATURE_IMAGE_MAX_LENGTH {
            return Err("Invalid signature field");
        }
        if self
            .prefill
            .as_deref()
            .is_some_and(|key| !PREFILL_KEYS.contains(&key))
            || self
                .label
                .as_deref()
                .is_some_and(|label| label.chars().count() > 60)
        {
            return Err("Invalid signature field");
        }
        let trimmed = value.trim();
        if self.kind == FieldKind::Signature {
            if !trimmed.is_empty() && !trimmed.starts_with(SIGNATURE_IMAGE_PREFIX) {
                return Err("Invalid signature image");
            }
        } else if trimmed.chars().count() > 500 {
            return Err("Value is too long");
        }
        Ok(())
    }
}

/// SignWell needs at least one signature left for the customer.
pub fn has_signer_signature(fields: &[SignatureField]) -> bool {
    fields
        .iter()
        .any(|field| field.kind == FieldKind::Signature && !field.is_stamped())
}

/// Why a PDF could not be prepared (`PdfSigningError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PdfSigningError {
    pub code: &'static str,
    pub message: String,
}

impl PdfSigningError {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

fn number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(value) =>
        {
            #[allow(clippy::cast_precision_loss)]
            Some(*value as f64)
        }
        Object::Real(value) => Some(f64::from(*value)),
        _ => None,
    }
}

fn resolve<'a>(doc: &'a Document, object: &'a Object) -> &'a Object {
    match object {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(object),
        other => other,
    }
}

/// A page attribute, from the page or the page tree above it.
fn inherited(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
    let mut node = doc.get_dictionary(page).ok()?;
    for _ in 0..64 {
        if let Ok(value) = node.get(key) {
            return Some(resolve(doc, value).clone());
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = doc.get_dictionary(parent).ok()?;
    }
    None
}

/// `[x1 y1 x2 y2]` as pdf-lib reads it: `(x, y, width, height)` from the
/// first corner.
fn rectangle(doc: &Document, object: &Object) -> Option<(f64, f64, f64, f64)> {
    let values: Vec<f64> = resolve(doc, object)
        .as_array()
        .ok()?
        .iter()
        .filter_map(|item| number(resolve(doc, item)))
        .collect();
    let [x1, y1, x2, y2] = values.as_slice() else {
        return None;
    };
    Some((*x1, *y1, x2 - x1, y2 - y1))
}

/// Where the viewer's page lies in PDF space (`signingPageGeometry`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    left: f64,
    bottom: f64,
    right: f64,
    top: f64,
    pub rotation: i64,
}

impl Geometry {
    pub fn of(doc: &Document, page: ObjectId) -> Result<Self, PdfSigningError> {
        let unsupported = || PdfSigningError::new("invalid_pdf", "Unsupported PDF page geometry");
        let media = inherited(doc, page, b"MediaBox")
            .and_then(|object| rectangle(doc, &object))
            .ok_or_else(unsupported)?;
        let crop = inherited(doc, page, b"CropBox")
            .and_then(|object| rectangle(doc, &object))
            .unwrap_or(media);
        let left = media.0.max(crop.0);
        let bottom = media.1.max(crop.1);
        let right = (media.0 + media.2).min(crop.0 + crop.2);
        let top = (media.1 + media.3).min(crop.1 + crop.3);
        #[allow(clippy::cast_possible_truncation)]
        let angle = inherited(doc, page, b"Rotate")
            .and_then(|object| number(&object))
            .unwrap_or(0.0) as i64;
        let rotation = angle.rem_euclid(360);
        let unit = doc
            .get_dictionary(page)
            .ok()
            .and_then(|node| node.get(b"UserUnit").ok())
            .and_then(|object| number(resolve(doc, object)))
            .unwrap_or(1.0);
        #[allow(clippy::float_cmp)]
        if right <= left || top <= bottom || ![0, 90, 180, 270].contains(&rotation) || unit != 1.0 {
            return Err(unsupported());
        }
        Ok(Self {
            left,
            bottom,
            right,
            top,
            rotation,
        })
    }

    pub fn width(&self) -> f64 {
        if self.rotation % 180 == 0 {
            self.right - self.left
        } else {
            self.top - self.bottom
        }
    }

    pub fn height(&self) -> f64 {
        if self.rotation % 180 == 0 {
            self.top - self.bottom
        } else {
            self.right - self.left
        }
    }

    /// A viewer point (from the top left) in PDF space.
    pub fn to_pdf(&self, x: f64, y: f64) -> (f64, f64) {
        match self.rotation {
            90 => (self.left + y, self.bottom + x),
            180 => (self.right - x, self.bottom + y),
            270 => (self.right - y, self.top - x),
            _ => (self.left + x, self.top - y),
        }
    }
}

/// The crop box (pdf-lib's `getCropBox`, which falls back to the media box).
fn crop_box(doc: &Document, page: ObjectId) -> Option<(f64, f64, f64, f64)> {
    inherited(doc, page, b"CropBox")
        .or_else(|| inherited(doc, page, b"MediaBox"))
        .and_then(|object| rectangle(doc, &object))
}

/// Helvetica only encodes WinAnsi: common typography folded, the rest
/// dropped (`toWinAnsiSafe`).
pub fn to_winansi_safe(value: &str) -> String {
    let folded: String = value
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => "'".to_string(),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => "\"".to_string(),
            '\u{2013}' | '\u{2014}' | '\u{2212}' => "-".to_string(),
            '\u{2026}' => "...".to_string(),
            '\u{00A0}' | '\t' => " ".to_string(),
            other => other.to_string(),
        })
        .collect();
    folded
        .nfc()
        .filter(|c| matches!(*c as u32, 0x20..=0x7E | 0xA1..=0xFF))
        .collect()
}

/// pdf-lib's font size that fits `lines` in the box (`fitFontSize`).
pub fn fit_font_size(lines: &[String], width: f64, height: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let count = lines.len().max(1) as f64;
    let by_height = (height - f64::from(BASELINE_PAD)) / count / f64::from(LINE_GAP);
    let mut size = by_height.clamp(f64::from(MIN_FONT_SIZE), f64::from(MAX_FONT_SIZE));
    let available = (width - f64::from(SIDE_PAD) * 2.0).max(1.0);
    for line in lines {
        let line_width = text_width(line, size);
        if line_width > available {
            size = f64::from(MIN_FONT_SIZE).max(size * (available / line_width));
        }
    }
    size
}

#[allow(clippy::cast_possible_truncation)]
fn text_width(text: &str, size: f64) -> f64 {
    f64::from(Font::Regular.width(text, size as f32))
}

fn signature_png(value: &str) -> Option<Vec<u8>> {
    let data = value.trim().strip_prefix(SIGNATURE_IMAGE_PREFIX)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

fn flate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// A page being added to: its own resources and drawing.
struct PageEdit {
    id: ObjectId,
    ops: Vec<u8>,
    xobjects: Vec<(Vec<u8>, ObjectId)>,
    fonts: Vec<(Vec<u8>, ObjectId)>,
}

impl PageEdit {
    const fn new(id: ObjectId) -> Self {
        Self {
            id,
            ops: Vec::new(),
            xobjects: Vec::new(),
            fonts: Vec::new(),
        }
    }
}

/// The page's resources as a dictionary of its own (inherited or shared
/// ones copied), so names can be added.
fn own_resources(doc: &Document, page: ObjectId) -> Dictionary {
    inherited(doc, page, b"Resources")
        .and_then(|object| object.as_dict().ok().cloned())
        .unwrap_or_default()
}

fn add_names(
    resources: &mut Dictionary,
    doc: &Document,
    kind: &[u8],
    names: &[(Vec<u8>, ObjectId)],
) {
    if names.is_empty() {
        return;
    }
    let mut dict = resources
        .get(kind)
        .ok()
        .map(|object| resolve(doc, object).clone())
        .and_then(|object| object.as_dict().ok().cloned())
        .unwrap_or_default();
    for (name, id) in names {
        dict.set(name.clone(), Object::Reference(*id));
    }
    resources.set(kind.to_vec(), Object::Dictionary(dict));
}

/// Wraps the page's drawing in `q … Q` and appends ours after it, like
/// pdf-lib does on the first edit of a page.
fn finish_page(doc: &mut Document, edit: &PageEdit) -> Option<()> {
    if edit.ops.is_empty() {
        return Some(());
    }
    let mut resources = own_resources(doc, edit.id);
    add_names(&mut resources, doc, b"XObject", &edit.xobjects);
    add_names(&mut resources, doc, b"Font", &edit.fonts);
    let mut contents: Vec<Object> = match doc.get_dictionary(edit.id).ok()?.get(b"Contents") {
        Ok(Object::Array(items)) => items.clone(),
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(items)) => items.clone(),
            _ => vec![Object::Reference(*id)],
        },
        _ => Vec::new(),
    };
    let open = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let mut ours = b"Q\n".to_vec();
    ours.extend_from_slice(&edit.ops);
    let mut close_dict = Dictionary::new();
    let compressed = flate(&ours);
    close_dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
    let close = doc.add_object(Stream::new(close_dict, compressed));
    contents.insert(0, Object::Reference(open));
    contents.push(Object::Reference(close));
    let page = doc.get_dictionary_mut(edit.id).ok()?;
    page.set("Contents", Object::Array(contents));
    page.set("Resources", Object::Dictionary(resources));
    Some(())
}

fn fmt(value: f64) -> String {
    let rounded = (value * 10_000.0).round() / 10_000.0;
    let text = format!("{rounded}");
    if text == "-0" { "0".into() } else { text }
}

/// `cos sin -sin cos` of a rotation in degrees.
const fn rotation_matrix(degrees: i64) -> (f64, f64) {
    match degrees.rem_euclid(360) {
        90 => (0.0, 1.0),
        180 => (-1.0, 0.0),
        270 => (0.0, -1.0),
        _ => (1.0, 0.0),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02X}");
            out
        })
}

/// The form's widgets per page, and the widget fields' appearance to draw.
struct Widget {
    page: ObjectId,
    id: ObjectId,
    rect: Option<(f64, f64, f64, f64)>,
    appearance: Option<ObjectId>,
}

/// Every terminal field of the form (`Fields`, through `Kids`).
fn form_widgets(doc: &Document) -> Vec<ObjectId> {
    let Some(form) = doc
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"AcroForm").ok())
        .map(|object| resolve(doc, object))
        .and_then(|object| object.as_dict().ok())
    else {
        return Vec::new();
    };
    let mut stack: Vec<ObjectId> = form
        .get(b"Fields")
        .ok()
        .map(|object| resolve(doc, object))
        .and_then(|object| object.as_array().ok())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_reference().ok())
                .collect()
        })
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut widgets = Vec::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Ok(node) = doc.get_dictionary(id) else {
            continue;
        };
        let kids: Vec<ObjectId> = node
            .get(b"Kids")
            .ok()
            .map(|object| resolve(doc, object))
            .and_then(|object| object.as_array().ok())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_reference().ok())
                    .collect()
            })
            .unwrap_or_default();
        let kids_are_fields = kids
            .iter()
            .any(|kid| doc.get_dictionary(*kid).is_ok_and(|kid| kid.has(b"T")));
        if kids_are_fields {
            stack.extend(kids);
        } else if kids.is_empty() {
            widgets.push(id);
        } else {
            widgets.extend(kids);
        }
    }
    widgets
}

/// The appearance pdf-lib draws for a widget: its normal appearance, or the
/// state `AS` names when there are several.
fn widget_appearance(doc: &Document, widget: ObjectId) -> Option<ObjectId> {
    let dict = doc.get_dictionary(widget).ok()?;
    let ap = resolve(doc, dict.get(b"AP").ok()?).as_dict().ok()?;
    let normal = ap.get(b"N").ok()?;
    let states = match normal {
        Object::Reference(id) => match doc.get_object(*id).ok()? {
            Object::Stream(_) => return Some(*id),
            Object::Dictionary(states) => states,
            _ => return None,
        },
        Object::Dictionary(states) => states,
        _ => return None,
    };
    // pdf-lib picks the on state when the field's value is on, else /Off.
    let value = dict
        .get(b"V")
        .ok()
        .or_else(|| {
            let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
            doc.get_dictionary(parent).ok()?.get(b"V").ok()
        })
        .map(|value| resolve(doc, value))
        .and_then(|value| value.as_name().ok());
    let state = match value {
        Some(value) if states.has(value) => value,
        Some(_) => b"Off".as_slice(),
        None => dict.get(b"AS").ok()?.as_name().ok()?,
    };
    states.get(state).ok()?.as_reference().ok()
}

/// Draws each form widget's appearance into its page and removes the form
/// (pdf-lib's `form.flatten()`); `strict` refuses widgets that are not the
/// form's own and appearances it cannot find.
#[allow(clippy::too_many_lines)]
fn flatten(
    doc: &mut Document,
    edits: &mut Vec<PageEdit>,
    strict: bool,
) -> Result<(), PdfSigningError> {
    let frozen = || PdfSigningError::new("invalid_pdf", "Interactive PDF fields cannot be frozen");
    let known: HashSet<ObjectId> = form_widgets(doc).into_iter().collect();
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut widgets = Vec::new();
    for page in &pages {
        let annotations: Vec<ObjectId> = doc
            .get_dictionary(*page)
            .ok()
            .and_then(|node| node.get(b"Annots").ok())
            .map(|object| resolve(doc, object))
            .and_then(|object| object.as_array().ok())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_reference().ok())
                    .collect()
            })
            .unwrap_or_default();
        for annotation in annotations {
            let is_widget = doc
                .get_dictionary(annotation)
                .ok()
                .and_then(|dict| dict.get(b"Subtype").ok())
                .and_then(|subtype| subtype.as_name().ok())
                == Some(b"Widget".as_slice());
            if !is_widget {
                continue;
            }
            if strict && !known.contains(&annotation) {
                return Err(frozen());
            }
            let rect = doc
                .get_dictionary(annotation)
                .ok()
                .and_then(|dict| dict.get(b"Rect").ok())
                .and_then(|object| rectangle(doc, object));
            widgets.push(Widget {
                page: *page,
                id: annotation,
                rect,
                appearance: widget_appearance(doc, annotation),
            });
        }
    }
    let mut counter = 0;
    for widget in &widgets {
        if !known.contains(&widget.id) {
            continue;
        }
        let (Some(appearance), Some((x, y, _, _))) = (widget.appearance, widget.rect) else {
            if strict {
                return Err(frozen());
            }
            continue;
        };
        let edit = edit_for(edits, widget.page);
        counter += 1;
        let name = format!("FlatWidget{counter}").into_bytes();
        edit.ops.extend_from_slice(
            format!(
                "q\n1 0 0 1 {} {} cm\n/{} Do\nQ\n",
                fmt(x),
                fmt(y),
                String::from_utf8_lossy(&name)
            )
            .as_bytes(),
        );
        edit.xobjects.push((name, appearance));
    }
    // The widgets leave the pages; the form goes.
    let removed: HashSet<ObjectId> = widgets.iter().map(|w| w.id).collect();
    for page in &pages {
        let annots = doc
            .get_dictionary(*page)
            .ok()
            .and_then(|node| node.get(b"Annots").ok())
            .cloned();
        let Some(annots) = annots else { continue };
        let items = match &annots {
            Object::Array(items) => items.clone(),
            Object::Reference(id) => doc
                .get_object(*id)
                .ok()
                .and_then(|object| object.as_array().ok().cloned())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let kept: Vec<Object> = items
            .into_iter()
            .filter(|item| {
                item.as_reference()
                    .map_or(true, |id| !removed.contains(&id))
            })
            .collect();
        if let Ok(node) = doc.get_dictionary_mut(*page) {
            if kept.is_empty() {
                node.remove(b"Annots");
            } else {
                node.set("Annots", Object::Array(kept));
            }
        }
    }
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"AcroForm");
    }
    Ok(())
}

fn edit_for(edits: &mut Vec<PageEdit>, page: ObjectId) -> &mut PageEdit {
    if let Some(index) = edits.iter().position(|edit| edit.id == page) {
        return &mut edits[index];
    }
    edits.push(PageEdit::new(page));
    edits.last_mut().expect("just pushed")
}

fn helvetica(doc: &mut Document) -> ObjectId {
    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    font.set("Subtype", Object::Name(b"Type1".to_vec()));
    font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
    font.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
    doc.add_object(font)
}

fn image_xobject(doc: &mut Document, image: &Image) -> ObjectId {
    let smask_id = image.alpha().map(|alpha| {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"XObject".to_vec()));
        dict.set("Subtype", Object::Name(b"Image".to_vec()));
        dict.set("Width", i64::from(image.width));
        dict.set("Height", i64::from(image.height));
        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
        dict.set("BitsPerComponent", 8);
        dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        doc.add_object(Stream::new(dict, flate(alpha)))
    });
    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", i64::from(image.width));
    dict.set("Height", i64::from(image.height));
    dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
    dict.set("BitsPerComponent", 8);
    dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
    if let Some(smask) = smask_id {
        dict.set("SMask", Object::Reference(smask));
    }
    doc.add_object(Stream::new(dict, flate(image.rgb())))
}

/// Prints the filled-in fields on the pages (`stampTextFieldsOnPdf`); how
/// many. `strict` (native signing) places by the viewer geometry and
/// refuses text it cannot print whole.
#[allow(clippy::too_many_lines)]
fn stamp(
    doc: &mut Document,
    edits: &mut Vec<PageEdit>,
    fields: &[SignatureField],
    strict: bool,
) -> Result<usize, PdfSigningError> {
    let stamps: Vec<&SignatureField> = fields.iter().filter(|field| field.is_stamped()).collect();
    if stamps.is_empty() {
        return Ok(0);
    }
    let pages = doc.get_pages();
    let font = helvetica(doc);
    let mut stamped = 0;
    let mut counter = 0;
    for field in stamps {
        let Some(page) = u32::try_from(field.page)
            .ok()
            .and_then(|n| pages.get(&n))
            .copied()
        else {
            continue;
        };
        let geometry = if strict {
            Some(Geometry::of(doc, page)?)
        } else {
            None
        };
        let Some(crop) = crop_box(doc, page) else {
            continue;
        };
        if field.kind == FieldKind::Signature {
            let Some(image) = signature_png(field.value.as_deref().unwrap_or(""))
                .and_then(|bytes| Image::decode(&bytes))
            else {
                continue;
            };
            let id = image_xobject(doc, &image);
            let scale =
                (field.width / f64::from(image.width)).min(field.height / f64::from(image.height));
            let (width, height) = (
                f64::from(image.width) * scale,
                f64::from(image.height) * scale,
            );
            let ((x, y), rotation) = geometry.map_or_else(
                || {
                    (
                        (crop.0 + field.x, crop.1 + crop.3 - (field.y + field.height)),
                        0,
                    )
                },
                |g| (g.to_pdf(field.x, field.y + field.height), g.rotation),
            );
            let (cos, sin) = rotation_matrix(rotation);
            counter += 1;
            let name = format!("SigImage{counter}").into_bytes();
            let edit = edit_for(edits, page);
            edit.ops.extend_from_slice(
                format!(
                    "q\n{} {} {} {} {} {} cm\n/{} Do\nQ\n",
                    fmt(width * cos),
                    fmt(width * sin),
                    fmt(-height * sin),
                    fmt(height * cos),
                    fmt(x),
                    fmt(y),
                    String::from_utf8_lossy(&name)
                )
                .as_bytes(),
            );
            edit.xobjects.push((name, id));
            stamped += 1;
            continue;
        }
        let raw = field.value.as_deref().unwrap_or("");
        let source = if strict {
            raw.to_string()
        } else {
            to_winansi_safe(raw)
        };
        let lines: Vec<String> = source
            .split('\n')
            .map(|line| line.trim_end_matches('\r').trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        let unprintable = || {
            PdfSigningError::new(
                "invalid_text",
                "Text cannot be printed without data loss; use supported characters and a larger field",
            )
        };
        let mut encoded = Vec::with_capacity(lines.len());
        for line in &lines {
            match Font::Regular.try_encode(line) {
                Some(bytes) => encoded.push(bytes),
                None if strict => return Err(unprintable()),
                None => encoded.push(crate::pdf::winansi(line)),
            }
        }
        let size = fit_font_size(&lines, field.width, field.height);
        #[allow(clippy::cast_precision_loss)]
        let count = lines.len() as f64;
        if strict
            && (size * count * f64::from(LINE_GAP) > field.height - f64::from(BASELINE_PAD)
                || lines
                    .iter()
                    .any(|line| text_width(line, size) > field.width - f64::from(SIDE_PAD) * 2.0))
        {
            return Err(unprintable());
        }
        let bottom = crop.1 + crop.3 - (field.y + field.height);
        counter += 1;
        let name = format!("SigFont{counter}").into_bytes();
        let edit = edit_for(edits, page);
        edit.fonts.push((name.clone(), font));
        for (index, bytes) in encoded.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let from_bottom = (lines.len() - 1 - index) as f64;
            let ((x, y), rotation) = geometry.map_or_else(
                || {
                    (
                        (
                            crop.0 + field.x + f64::from(SIDE_PAD),
                            bottom
                                + f64::from(BASELINE_PAD)
                                + from_bottom * size * f64::from(LINE_GAP),
                        ),
                        0,
                    )
                },
                |g| {
                    (
                        g.to_pdf(
                            field.x + f64::from(SIDE_PAD),
                            field.y + field.height
                                - f64::from(BASELINE_PAD)
                                - from_bottom * size * f64::from(LINE_GAP),
                        ),
                        g.rotation,
                    )
                },
            );
            let (cos, sin) = rotation_matrix(rotation);
            edit.ops.extend_from_slice(
                format!(
                    "q\nBT\n{TEXT_COLOR}\n/{} {} Tf\n{} {} {} {} {} {} Tm\n<{}> Tj\nET\nQ\n",
                    String::from_utf8_lossy(&name),
                    fmt(size),
                    fmt(cos),
                    fmt(sin),
                    fmt(-sin),
                    fmt(cos),
                    fmt(x),
                    fmt(y),
                    hex(bytes)
                )
                .as_bytes(),
            );
        }
        stamped += 1;
    }
    Ok(stamped)
}

fn save(doc: &mut Document) -> Result<Vec<u8>, PdfSigningError> {
    let mut out = Vec::new();
    doc.save_to(&mut out)
        .map_err(|_| PdfSigningError::new("invalid_pdf", "This PDF cannot be saved"))?;
    Ok(out)
}

fn apply(doc: &mut Document, edits: &[PageEdit]) -> Result<(), PdfSigningError> {
    for edit in edits {
        finish_page(doc, edit)
            .ok_or_else(|| PdfSigningError::new("invalid_pdf", "This PDF page cannot be edited"))?;
    }
    Ok(())
}

/// A PDF ready for `SignWell`, and whether we printed text on it.
#[derive(Debug, Clone)]
pub struct SignWellPdf {
    pub bytes: Vec<u8>,
    pub stamped_text: bool,
}

/// Freezes the form and prints the sender's fields (`preparePdfForSignWell`
/// with placed fields). A PDF we cannot read goes as it is, unless the
/// sender filled fields that would then be lost.
pub fn prepare_for_signwell(
    bytes: &[u8],
    fields: &[SignatureField],
) -> Result<SignWellPdf, PdfSigningError> {
    let wants_text = fields.iter().any(SignatureField::is_stamped);
    let attempt = || -> Result<SignWellPdf, PdfSigningError> {
        let mut doc = Document::load_mem(bytes)
            .map_err(|_| PdfSigningError::new("invalid_pdf", "unreadable"))?;
        let mut edits = Vec::new();
        // SignWell can still overlay fields on an interactive PDF.
        let mut flattened_edits = Vec::new();
        if flatten(&mut doc, &mut flattened_edits, false).is_ok() {
            edits = flattened_edits;
        }
        let stamped = stamp(&mut doc, &mut edits, fields, false)?;
        apply(&mut doc, &edits)?;
        Ok(SignWellPdf {
            bytes: save(&mut doc)?,
            stamped_text: stamped > 0,
        })
    };
    match attempt() {
        Ok(prepared) => Ok(prepared),
        Err(_) if !wants_text => Ok(SignWellPdf {
            bytes: bytes.to_vec(),
            stamped_text: false,
        }),
        Err(_) => Err(PdfSigningError::new(
            "invalid_pdf",
            "Could not fill in the fields",
        )),
    }
}

/// The PDF the signer will see, its hash and the fields with stable ids.
#[derive(Debug, Clone)]
pub struct NativePrepared {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub fields: Vec<SignatureField>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes)).to_lowercase()
}

/// Every page renders (`verifyRenderable`, here with `hayro`).
pub fn renderable(bytes: &[u8]) -> bool {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
    let Ok(pdf) = Pdf::new(std::sync::Arc::new(bytes.to_vec())) else {
        return false;
    };
    let pages = pdf.pages();
    if pages.is_empty() {
        return false;
    }
    let cache = RenderCache::new();
    pages.iter().all(|page| {
        let (width, height) = page.render_dimensions();
        if width <= 0.0 || height <= 0.0 {
            return false;
        }
        let scale = (768.0 / width.max(height)).min(1.0);
        let pixmap = render(
            page,
            &cache,
            &InterpreterSettings::default(),
            &RenderSettings::default(),
            &PixmapSettings {
                x_scale: scale,
                y_scale: scale,
                bg_color: WHITE,
            },
        );
        pixmap.width() > 0 && pixmap.height() > 0
    })
}

fn valid_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    let [month, day, year] = parts.as_slice() else {
        return false;
    };
    if month.len() != 2
        || day.len() != 2
        || year.len() != 4
        || !value.chars().all(|c| c.is_ascii_digit() || c == '/')
    {
        return false;
    }
    let (Ok(month), Ok(day), Ok(year)) = (
        month.parse::<u32>(),
        day.parse::<u32>(),
        year.parse::<i32>(),
    ) else {
        return false;
    };
    year > 0 && chrono::NaiveDate::from_ymd_opt(year, month, day).is_some()
}

fn valid_png(value: &str) -> bool {
    let Some(encoded) = value.strip_prefix(SIGNATURE_IMAGE_PREFIX) else {
        return false;
    };
    if value.len() > SIGNATURE_IMAGE_MAX_LENGTH || encoded.len() % 4 != 0 {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    if !bytes.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]) {
        return false;
    }
    let Ok(reader) =
        image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png)
            .into_dimensions()
    else {
        return false;
    };
    let (width, height) = reader;
    width > 0
        && height > 0
        && width <= 4096
        && height <= 4096
        && u64::from(width) * u64::from(height) <= 4_000_000
        && image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).is_ok()
}

/// The sender's values are printable (`validateValues`).
pub fn validate_values(fields: &[SignatureField]) -> Result<(), PdfSigningError> {
    for field in fields {
        let value = field.value.as_deref().unwrap_or("");
        if value.trim().is_empty() {
            continue;
        }
        if field.kind == FieldKind::Signature {
            if !valid_png(value) {
                return Err(PdfSigningError::new(
                    "invalid_image",
                    "Signature must be a valid PNG of at most 4096 pixels per side and 4 million pixels",
                ));
            }
        } else if value.encode_utf16().count() > 500
            || (field.kind == FieldKind::Date && !valid_date(value))
        {
            return Err(PdfSigningError::new(
                "invalid_answers",
                "Dates must be valid MM/DD/YYYY values and text must be at most 500 characters",
            ));
        }
    }
    Ok(())
}

fn load_strict(bytes: &[u8]) -> Result<Document, PdfSigningError> {
    let invalid = || {
        PdfSigningError::new(
            "invalid_pdf",
            "This PDF cannot be safely prepared for native signing",
        )
    };
    let doc = Document::load_mem(bytes).map_err(|_| invalid())?;
    let pages = doc.get_pages();
    if pages.is_empty() {
        return Err(invalid());
    }
    for page in pages.values() {
        Geometry::of(&doc, *page).map_err(|_| invalid())?;
    }
    let xfa = doc
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"AcroForm").ok())
        .map(|object| resolve(&doc, object))
        .and_then(|object| object.as_dict().ok())
        .is_some_and(|form| form.has(b"XFA"));
    // Digital certificates cannot survive our changes.
    let signed = doc.objects.values().any(|object| match object {
        Object::Dictionary(dict) => dict.has(b"ByteRange"),
        Object::Stream(stream) => stream.dict.has(b"ByteRange"),
        _ => false,
    });
    if xfa || signed {
        return Err(invalid());
    }
    Ok(doc)
}

/// The fields with stable ids, within their pages (`checkedFields`).
fn checked_fields(
    doc: &Document,
    fields: &[SignatureField],
) -> Result<Vec<SignatureField>, PdfSigningError> {
    let invalid = |message: &str| PdfSigningError::new("invalid_fields", message);
    let with_ids: Vec<SignatureField> = fields
        .iter()
        .map(|field| SignatureField {
            id: field
                .id
                .clone()
                .or_else(|| Some(uuid::Uuid::new_v4().to_string())),
            ..field.clone()
        })
        .collect();
    let ids: HashSet<&str> = with_ids
        .iter()
        .filter_map(|field| field.id.as_deref())
        .collect();
    if with_ids.is_empty()
        || with_ids.len() > 20
        || ids.len() != with_ids.len()
        || with_ids.iter().any(|field| {
            field.check().is_err()
                || field
                    .id
                    .as_deref()
                    .is_some_and(|id| id.is_empty() || id.len() > 64)
        })
        || !has_signer_signature(&with_ids)
    {
        return Err(invalid(
            "Invalid signing fields or missing signer signature",
        ));
    }
    let pages = doc.get_pages();
    for field in &with_ids {
        let page = u32::try_from(field.page)
            .ok()
            .and_then(|n| pages.get(&n))
            .ok_or_else(|| invalid("Signing field page does not exist"))?;
        let geometry = Geometry::of(doc, *page)?;
        if field.x + field.width > geometry.width() || field.y + field.height > geometry.height() {
            return Err(invalid("Signing field exceeds the displayed page bounds"));
        }
        if field.kind == FieldKind::AutofillDateSigned && !field.trimmed_value().is_empty() {
            return Err(invalid("Signed date must be supplied by the server"));
        }
    }
    Ok(with_ids)
}

/// The document the signer will see: the form frozen, the sender's fields
/// printed, every page renderable (`prepareNativeSignaturePdf`).
pub fn prepare_native(
    bytes: &[u8],
    placed: &[SignatureField],
) -> Result<NativePrepared, PdfSigningError> {
    let mut doc = load_strict(bytes)?;
    if !renderable(bytes) {
        return Err(PdfSigningError::new(
            "invalid_pdf",
            "This PDF cannot be rendered for native signing",
        ));
    }
    let fields = checked_fields(&doc, placed)?;
    validate_values(&fields)?;
    let mut edits = Vec::new();
    flatten(&mut doc, &mut edits, true)?;
    stamp(&mut doc, &mut edits, &fields, true)?;
    apply(&mut doc, &edits)?;
    let out = save(&mut doc)?;
    if !renderable(&out) {
        return Err(PdfSigningError::new(
            "invalid_pdf",
            "Prepared PDF cannot be rendered",
        ));
    }
    Ok(NativePrepared {
        sha256: sha256_hex(&out),
        bytes: out,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typography_is_folded_for_helvetica() {
        assert_eq!(to_winansi_safe("“Ann’s” — 3…\u{1F600}"), "\"Ann's\" - 3...");
        assert_eq!(to_winansi_safe("Café"), "Café");
    }

    #[test]
    fn the_font_shrinks_to_fit() {
        assert!((fit_font_size(&["Ann".into()], 200.0, 18.0) - 11.0).abs() < 1e-9);
        let long = fit_font_size(&["A very long customer name indeed".into()], 60.0, 18.0);
        assert!((long - 6.0).abs() < 1e-9);
        let two = fit_font_size(&["a".into(), "b".into()], 200.0, 20.0);
        assert!((two - 7.826_086_956_521_74).abs() < 1e-6);
    }

    #[test]
    fn dates_are_real_calendar_days() {
        assert!(valid_date("02/29/2024"));
        assert!(!valid_date("02/30/2024"));
        assert!(!valid_date("2/3/2024"));
    }

    #[test]
    fn geometry_follows_rotation() {
        let g = Geometry {
            left: 0.0,
            bottom: 0.0,
            right: 612.0,
            top: 792.0,
            rotation: 90,
        };
        assert!((g.width() - 792.0).abs() < 1e-9);
        assert_eq!(g.to_pdf(10.0, 20.0), (20.0, 10.0));
        let g = Geometry { rotation: 0, ..g };
        assert_eq!(g.to_pdf(10.0, 20.0), (10.0, 772.0));
    }
}
