//! A one-page PDF with the standard Helvetica fonts, lines and images:
//! enough for the documents the CRM drew with pdf-lib (the post-install
//! checklist).

use crate::pdf_fonts::{HELVETICA, HELVETICA_BOLD};
use image::GenericImageView;
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, Str};

/// pdf-lib's default page (A4).
pub const PAGE_WIDTH: f32 = 595.28;
pub const PAGE_HEIGHT: f32 = 841.89;

/// `text` in WinAnsi, as Helvetica draws it; missing characters become `?`.
pub fn winansi(text: &str) -> Vec<u8> {
    Font::Regular.encode(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    const fn table(self) -> &'static [(char, u8, u16)] {
        match self {
            Self::Regular => &HELVETICA,
            Self::Bold => &HELVETICA_BOLD,
        }
    }

    const fn resource(self) -> Name<'static> {
        match self {
            Self::Regular => Name(b"F1"),
            Self::Bold => Name(b"F2"),
        }
    }

    fn glyph(self, ch: char) -> (u8, u16) {
        self.table()
            .iter()
            .find(|(glyph, _, _)| *glyph == ch)
            .map_or((b'?', 556), |(_, code, width)| (*code, *width))
    }

    /// pdf-lib's `widthOfTextAtSize`.
    pub fn width(self, text: &str, size: f32) -> f32 {
        let units: u32 = text.chars().map(|ch| u32::from(self.glyph(ch).1)).sum();
        #[allow(clippy::cast_precision_loss)]
        let units = units as f32;
        units * size / 1000.0
    }

    /// WinAnsi bytes, or `None` when the font lacks a character (pdf-lib's
    /// `encodeText` throws).
    pub fn try_encode(self, text: &str) -> Option<Vec<u8>> {
        text.chars()
            .map(|ch| {
                self.table()
                    .iter()
                    .find(|(glyph, _, _)| *glyph == ch)
                    .map(|(_, code, _)| *code)
            })
            .collect()
    }

    /// WinAnsi bytes; characters the font lacks print as `?`.
    fn encode(self, text: &str) -> Vec<u8> {
        text.chars().map(|ch| self.glyph(ch).0).collect()
    }
}

/// A decoded image ready to place.
pub struct Image {
    /// Pixels wide.
    pub width: u32,
    /// Pixels high.
    pub height: u32,
    rgb: Vec<u8>,
    alpha: Option<Vec<u8>>,
}

impl Image {
    /// Red, green and blue bytes, row by row.
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// The alpha bytes, when not fully opaque.
    pub fn alpha(&self) -> Option<&[u8]> {
        self.alpha.as_deref()
    }

    /// A PNG or JPEG (anything the `image` crate reads); `None` when unreadable.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let decoded = image::load_from_memory(bytes).ok()?;
        let (width, height) = decoded.dimensions();
        let rgba = decoded.to_rgba8();
        let alpha: Vec<u8> = rgba.pixels().map(|pixel| pixel.0[3]).collect();
        let opaque = alpha.iter().all(|value| *value == 255);
        let rgb = rgba
            .pixels()
            .flat_map(|pixel| [pixel.0[0], pixel.0[1], pixel.0[2]])
            .collect();
        Some(Self {
            width,
            height,
            rgb,
            alpha: (!opaque).then_some(alpha),
        })
    }
}

/// One page being drawn, pdf-lib style (origin bottom left, points).
pub struct Page {
    content: Content,
    images: Vec<(Image, Name<'static>)>,
}

const IMAGE_NAMES: [&[u8]; 8] = [
    b"Im0", b"Im1", b"Im2", b"Im3", b"Im4", b"Im5", b"Im6", b"Im7",
];

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

impl Page {
    pub fn new() -> Self {
        Self {
            content: Content::new(),
            images: Vec::new(),
        }
    }

    pub fn text(&mut self, text: &str, x: f32, y: f32, size: f32, font: Font) {
        self.content
            .begin_text()
            .set_font(font.resource(), size)
            .next_line(x, y)
            .show(Str(&font.encode(text)))
            .end_text();
    }

    pub fn rectangle(&mut self, x: f32, y: f32, width: f32, height: f32, border: f32) {
        self.content
            .set_line_width(border)
            .rect(x, y, width, height)
            .stroke();
    }

    pub fn line(&mut self, from: (f32, f32), to: (f32, f32), thickness: f32) {
        self.content
            .set_line_width(thickness)
            .move_to(from.0, from.1)
            .line_to(to.0, to.1)
            .stroke();
    }

    /// Draws `image` stretched to `width` × `height` with its bottom left at
    /// (`x`, `y`). At most eight images per page.
    pub fn image(&mut self, image: Image, x: f32, y: f32, width: f32, height: f32) {
        let Some(name) = IMAGE_NAMES.get(self.images.len()) else {
            return;
        };
        let name = Name(name);
        self.content
            .save_state()
            .transform([width, 0.0, 0.0, height, x, y])
            .x_object(name)
            .restore_state();
        self.images.push((image, name));
    }

    /// The finished document.
    pub fn finish(self) -> Vec<u8> {
        let catalog = Ref::new(1);
        let tree = Ref::new(2);
        let page = Ref::new(3);
        let contents = Ref::new(4);
        let regular = Ref::new(5);
        let bold = Ref::new(6);
        let mut next = 7;
        let mut alloc = || {
            let id = Ref::new(next);
            next += 1;
            id
        };
        let mut placed = Vec::new();
        for (image, name) in self.images {
            let id = alloc();
            let mask = image.alpha.as_ref().map(|_| alloc());
            placed.push((image, name, id, mask));
        }
        let mut pdf = Pdf::new();
        pdf.catalog(catalog).pages(tree);
        pdf.pages(tree).kids([page]).count(1);
        {
            let mut writer = pdf.page(page);
            writer
                .parent(tree)
                .media_box(Rect::new(0.0, 0.0, PAGE_WIDTH, PAGE_HEIGHT))
                .contents(contents);
            let mut resources = writer.resources();
            resources
                .fonts()
                .pair(Font::Regular.resource(), regular)
                .pair(Font::Bold.resource(), bold);
            let mut objects = resources.x_objects();
            for (_, name, id, _) in &placed {
                objects.pair(*name, *id);
            }
            objects.finish();
            resources.finish();
            writer.finish();
        }
        for (id, base) in [
            (regular, Name(b"Helvetica")),
            (bold, Name(b"Helvetica-Bold")),
        ] {
            pdf.type1_font(id)
                .base_font(base)
                .encoding_predefined(Name(b"WinAnsiEncoding"));
        }
        for (image, _, id, mask) in &placed {
            let samples = miniz_oxide::deflate::compress_to_vec_zlib(&image.rgb, 6);
            let mut xobject = pdf.image_xobject(*id, &samples);
            xobject.filter(Filter::FlateDecode);
            xobject.width(i32::try_from(image.width).unwrap_or(i32::MAX));
            xobject.height(i32::try_from(image.height).unwrap_or(i32::MAX));
            xobject.color_space_name(Name(b"DeviceRGB"));
            xobject.bits_per_component(8);
            if let Some(mask) = mask {
                xobject.s_mask(*mask);
            }
            xobject.finish();
            if let (Some(mask), Some(alpha)) = (mask, &image.alpha) {
                let alpha = miniz_oxide::deflate::compress_to_vec_zlib(alpha, 6);
                let mut soft = pdf.image_xobject(*mask, &alpha);
                soft.filter(Filter::FlateDecode);
                soft.width(i32::try_from(image.width).unwrap_or(i32::MAX));
                soft.height(i32::try_from(image.height).unwrap_or(i32::MAX));
                soft.color_space_name(Name(b"DeviceGray"));
                soft.bits_per_component(8);
                soft.finish();
            }
        }
        let content = self.content.finish();
        pdf.stream(contents, &content);
        pdf.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Font;

    #[test]
    fn widths_follow_pdf_lib() {
        // pdf-lib: Helvetica "Hello" at 12 pt is 27.336 points wide.
        assert!((Font::Regular.width("Hello", 12.0) - 27.336).abs() < 0.001);
    }
}
