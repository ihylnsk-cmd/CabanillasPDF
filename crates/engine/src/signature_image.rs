//! Local image signatures, shared by Fill & Sign and its automation tool.

use std::io::{Cursor, Read};
use std::sync::Arc;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use pdfcraft_render::PageInfo;

use crate::{Document, Edit, MarkFile, guard};

/// Read-only layers for interactive movement: the page without this signature, and its image.
/// Preparing them never changes the working bytes, dirty flag or undo history.
pub struct ImageSignaturePreview {
    pub image: SignatureImage,
    pub opacity: f32,
    /// The page `/Rotate` the picture is drawn turned back by (see [`pdfcraft_annot::picture_rotation`]).
    pub turn: i64,
    page: usize,
    bytes: Arc<Vec<u8>>,
    config: pdfcraft_render::RenderConfig,
}

impl ImageSignaturePreview {
    /// One background worker; the UI reuses its page texture throughout a gesture.
    pub fn renderer(&self) -> pdfcraft_render::RenderPool {
        pdfcraft_render::RenderPool::new(self.bytes.clone(), 1, self.config.clone())
    }

    /// Synchronous counterpart for automation, with the renderer's normal pixel limits.
    pub fn render_background(&self, scale: f32) -> Result<pdfcraft_render::RenderedPage, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("preview scale must be finite and positive".into());
        }
        guard(|| {
            let mut renderer = pdfcraft_render::PageRenderer::new(self.bytes.clone(), self.config.clone());
            let out = renderer.render(pdfcraft_render::RenderRequest { page: self.page, scale, ..Default::default() });
            if let Some(e) = &out.error {
                return Err(e.clone());
            }
            Ok(out)
        })?
    }
}

impl Document {
    /// Prepare the actual embedded signature, including alpha, and an independent background.
    /// A cheap COS clone excludes only this annotation; other page content remains visible.
    pub fn image_signature_preview(&self, page: usize, index: usize) -> Result<Option<ImageSignaturePreview>, String> {
        guard(|| {
            let editor = self.editor.as_ref().ok_or("the document can't be read")?;
            let Some(picture) = pdfcraft_annot::signature_image(&editor.cos, page, index).map_err(|e| e.to_string())? else { return Ok(None) };
            let obj = editor.cos.get(picture);
            let pdfcraft_cos::Object::Stream(s) = &*obj else { return Err("the signature image is damaged".into()) };
            let (w, h) = (s.dict.int(b"Width").unwrap_or(0), s.dict.int(b"Height").unwrap_or(0));
            if w <= 0 || h <= 0 || w > 4096 || h > 4096 || w.saturating_mul(h) > MAX_PIXELS as i64 {
                return Err(SignatureImageError::Size.to_string());
            }
            let (_, bytes) = pdfcraft_create::image_file(&editor.cos, picture)?;
            let image = SignatureImage::from_bytes(&bytes).map_err(|e| e.to_string())?;
            let opacity =
                pdfcraft_annot::props(&editor.cos, page, index).map(|p| p.opacity).filter(|v| v.is_finite()).unwrap_or(1.0).clamp(0.0, 1.0) as f32;
            let turn = pdfcraft_annot::picture_rotation(&editor.cos, page, index).map_err(|e| e.to_string())?;
            let mut cos = editor.cos.clone();
            pdfcraft_annot::delete_annotation(&mut cos, page, index).map_err(|e| e.to_string())?;
            let bytes = Arc::new(pdfcraft_cos::write_incremental(&cos, &pdfcraft_cos::SaveOptions::default()).map_err(|e| e.to_string())?);
            Ok(Some(ImageSignaturePreview { image, opacity, turn, page, bytes, config: self.config.clone() }))
        })?
    }
}

/// The user-space box `width` x `height` points as displayed on a page turned by `rotation` (its
/// `/Rotate`), with its left edge at `at` (a user-space point) and vertically centred on it. On a
/// quarter-turned page it is `height` wide and `width` tall in user space. `None` for a non-finite
/// point.
pub fn upright_box(rotation: i64, at: [f64; 2], width: f64, height: f64) -> Option<[f64; 4]> {
    if !at.iter().chain(&[width, height]).all(|v| v.is_finite()) {
        return None;
    }
    // Right and up as displayed, as user-space vectors: the page's view matrix without its offset.
    let [a, b, c, d, ..] = pdfcraft_model::view_matrix_for(rotation, [0.0; 4]);
    let (right, up) = ([a * width, b * width], [c * height / 2.0, d * height / 2.0]);
    Some([
        at[0] + right[0].min(0.0) - up[0].abs(),
        at[1] + right[1].min(0.0) - up[1].abs(),
        at[0] + right[0].max(0.0) + up[0].abs(),
        at[1] + right[1].max(0.0) + up[1].abs(),
    ])
}

/// Saved signatures live in settings, so both source and normalized PNG are bounded.
pub const MAX_SIGNATURE_IMAGE_BYTES: usize = 4 << 20;
const MAX_PIXELS: u64 = 4 << 20;

/// A photo or scan of a signature ([`SignatureImage::from_picture`]) is read within wider limits
/// and then shrunk: phone cameras take 12 to 50 megapixels.
pub const MAX_PICTURE_BYTES: usize = 32 << 20;
const MAX_PICTURE_SIDE: u32 = 8192;
const MAX_PICTURE_PIXELS: u64 = 52 << 20;
/// The longest side kept from a picture: over 900 pixels per inch across the 150 pt signature box.
const PICTURE_SIDE: u32 = 2000;

/// What a source image may ask of the decoder.
struct Bounds {
    bytes: usize,
    side: u32,
    pixels: u64,
    alloc: u64,
    too_big: fn() -> SignatureImageError,
}

const STORED: Bounds =
    Bounds { bytes: MAX_SIGNATURE_IMAGE_BYTES, side: 4096, pixels: MAX_PIXELS, alloc: 32 << 20, too_big: || SignatureImageError::Size };
/// 52 megapixels decode to 156 MiB of RGB (JPEG) or 208 MiB of RGBA (PNG).
const PICTURE: Bounds = Bounds {
    bytes: MAX_PICTURE_BYTES,
    side: MAX_PICTURE_SIDE,
    pixels: MAX_PICTURE_PIXELS,
    alloc: 256 << 20,
    too_big: || SignatureImageError::PictureSize,
};

/// Decode a PNG or JPEG within `bounds`, upright (EXIF orientation applied).
fn decode(bytes: &[u8], bounds: &Bounds) -> Result<DynamicImage, SignatureImageError> {
    if bytes.len() > bounds.bytes {
        return Err((bounds.too_big)());
    }
    let format = image::guess_format(bytes).map_err(|_| SignatureImageError::Format)?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(SignatureImageError::Format);
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(bounds.side);
    limits.max_image_height = Some(bounds.side);
    limits.max_alloc = Some(bounds.alloc);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| sized(e, bounds))?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || w > bounds.side || h > bounds.side || u64::from(w) * u64::from(h) > bounds.pixels {
        return Err((bounds.too_big)());
    }
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| sized(e, bounds))?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// A decoder that stopped at one of `bounds`' limits says the picture is too big, in the words
/// of the limit; anything else is a decoding failure.
fn sized(error: image::ImageError, bounds: &Bounds) -> SignatureImageError {
    match error {
        image::ImageError::Limits(_) => (bounds.too_big)(),
        other => SignatureImageError::Decode(other),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SignatureImageError {
    #[error("Choose a PNG or JPEG image.")]
    Format,
    #[error("Choose an image smaller than 4 MiB and 4 megapixels (at most 4096 pixels per side).")]
    Size,
    #[error("Choose a picture smaller than 32 MiB and 50 megapixels (at most 8192 pixels per side).")]
    PictureSize,
    #[error("Couldn't read the signature image: {0}")]
    Read(#[from] std::io::Error),
    #[error("Couldn't decode the signature image: {0}")]
    Decode(#[from] image::ImageError),
}

/// A validated image, normalized to PNG without source paths or metadata. Alpha is preserved.
#[derive(Clone, Debug, PartialEq)]
pub struct SignatureImage {
    bytes: Arc<Vec<u8>>,
    rgba: Arc<Vec<u8>>,
    size: [usize; 2],
}

impl SignatureImage {
    /// Read at most the file limit plus one byte, even if the file grows while being read.
    pub fn read(reader: impl Read) -> Result<Self, SignatureImageError> {
        let mut bytes = Vec::new();
        reader.take(MAX_SIGNATURE_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SignatureImageError> {
        let rgba = decode(bytes, &STORED)?.into_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(rgba.clone()).write_to(&mut png, ImageFormat::Png)?;
        let bytes = png.into_inner();
        if bytes.len() > MAX_SIGNATURE_IMAGE_BYTES {
            return Err(SignatureImageError::Size);
        }
        Ok(Self { bytes: Arc::new(bytes), rgba: Arc::new(rgba.into_raw()), size })
    }

    /// [`Self::from_picture`] for a file, reading at most the picture limit plus one byte.
    pub fn read_picture(reader: impl Read) -> Result<Self, SignatureImageError> {
        let mut bytes = Vec::new();
        reader.take(MAX_PICTURE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        Self::from_picture(&bytes)
    }

    /// A photo or scan of a signature made ready to place: a large picture is shrunk, the paper
    /// behind the ink becomes transparent whatever its shade and however unevenly it is lit,
    /// marks away from the signature (the edge of the sheet, a desk) are dropped, and the result
    /// is cropped to the signature. A picture that already has a transparent background keeps
    /// it and is only cropped.
    pub fn from_picture(bytes: &[u8]) -> Result<Self, SignatureImageError> {
        let image = decode(bytes, &PICTURE)?;
        let image = if image.width().max(image.height()) > PICTURE_SIDE { image.thumbnail(PICTURE_SIDE, PICTURE_SIDE) } else { image };
        let mut rgba = crate::signature_paper::prepare(image.into_rgba8());
        // Mostly transparent pictures compress far below the stored limit; one that doesn't is
        // shrunk until it fits.
        for _ in 0..6 {
            let mut png = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba.clone()).write_to(&mut png, ImageFormat::Png)?;
            let bytes = png.into_inner();
            if bytes.len() <= MAX_SIGNATURE_IMAGE_BYTES {
                let size = [rgba.width() as usize, rgba.height() as usize];
                return Ok(Self { bytes: Arc::new(bytes), rgba: Arc::new(rgba.into_raw()), size });
            }
            let (w, h) = ((rgba.width() * 7 / 10).max(1), (rgba.height() * 7 / 10).max(1));
            rgba = image::imageops::thumbnail(&rgba, w, h);
        }
        Err(SignatureImageError::PictureSize)
    }

    pub fn bytes(&self) -> &Arc<Vec<u8>> {
        &self.bytes
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn size(&self) -> [usize; 2] {
        self.size
    }

    /// Make near-white paper transparent, keeping darker ink.
    pub fn remove_white_background(&self, cutoff: u8, feather: u8) -> Result<Self, SignatureImageError> {
        let [w, h] = self.size;
        let mut rgba = image::RgbaImage::from_raw(w as u32, h as u32, self.rgba.as_ref().clone()).ok_or(SignatureImageError::Size)?;
        let high = f32::from(cutoff);
        let span = f32::from(feather.max(1));
        for pixel in rgba.pixels_mut() {
            let [r, g, b, a] = pixel.0;
            let whiteness = f32::from(r.min(g).min(b));
            let keep = ((high - whiteness) / span).clamp(0.0, 1.0);
            pixel.0[3] = (f32::from(a) * keep).round() as u8;
        }
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(rgba).write_to(&mut png, ImageFormat::Png)?;
        Self::from_bytes(&png.into_inner())
    }

    /// The user-space box for the image with its left edge at `at` (a user-space point on `page`),
    /// vertically centered, with the same 150 pt width cap as typed names. Left and centered are
    /// as displayed: on a page with `/Rotate` the box runs along the turned page's axes.
    pub fn rect(&self, page: &PageInfo, at: [f64; 2], initials: bool) -> Option<[f64; 4]> {
        if !at.iter().all(|v| v.is_finite()) {
            return None;
        }
        let [w, h] = self.size;
        let scale = (150.0 / w as f64).min(if initials { 24.0 } else { 32.0 } / h as f64);
        upright_box(i64::from(page.rotation), at, w as f64 * scale, h as f64 * scale)
    }

    pub fn edit(&self, page: usize, info: &PageInfo, at: [f64; 2], initials: bool, author: &str) -> Option<Edit> {
        let rect = self.rect(info, at, initials)?;
        let label = if initials { "Add initials" } else { "Add signature" };
        Some(Edit::Batch {
            label: label.into(),
            edits: vec![Edit::AddCustomStamp {
                page,
                rect,
                name: if initials { "Initials" } else { "Signature" }.into(),
                file: MarkFile { name: "signature.png".into(), bytes: self.bytes.clone(), page: 0 },
                author: author.into(),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 300 x 400 page shown turned `rotation` degrees clockwise.
    fn page(rotation: u16) -> PageInfo {
        let (width, height) = if rotation.is_multiple_of(180) { (300.0, 400.0) } else { (400.0, 300.0) };
        PageInfo { width, height, label: String::new(), crop: [0.0, 0.0, 300.0, 400.0], rotation }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(w, h, image::Rgba([40, 60, 80, 128]));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn image_signatures_preserve_alpha_and_fit_the_signature_box() {
        let image = SignatureImage::from_bytes(&png(400, 40)).unwrap();
        assert_eq!(image.size(), [400, 40]);
        assert_eq!(&image.rgba()[..4], &[40, 60, 80, 128]);
        let Edit::Batch { edits, .. } = image.edit(0, &page(0), [20.0, 100.0], false, "Ada").unwrap() else { panic!() };
        let Edit::AddCustomStamp { rect, .. } = &edits[0] else { panic!() };
        assert_eq!(*rect, [20.0, 92.5, 170.0, 107.5]);
        assert!(image.edit(0, &page(0), [f64::NAN, 0.0], false, "").is_none());
        assert_eq!(SignatureImage::from_bytes(image.bytes()).unwrap(), image);
    }

    #[test]
    fn invalid_and_oversized_signature_images_are_rejected() {
        assert!(SignatureImage::from_bytes(b"not an image").is_err());
        assert!(SignatureImage::from_bytes(b"\x89PNG\r\n\x1a\ntruncated").is_err());
        assert!(SignatureImage::from_bytes(&vec![0; MAX_SIGNATURE_IMAGE_BYTES + 1]).is_err());
        assert!(SignatureImage::from_bytes(&png(4097, 1)).is_err());
        assert!(SignatureImage::from_bytes(&png(4096, 1025)).is_err());
        assert!(SignatureImage::read(std::io::repeat(0)).is_err());
        assert!(matches!(SignatureImage::from_bytes(b"GIF89a\x02\0\x02\0"), Err(SignatureImageError::Format)));
    }

    #[test]
    fn jpeg_signatures_are_normalized_to_png() {
        let mut jpeg = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(60, 20, image::Rgb([20, 20, 20])).write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        let image = SignatureImage::from_bytes(&jpeg.into_inner()).unwrap();
        assert_eq!(image.size(), [60, 20]);
        assert!(image.bytes().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(image.rgba().as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn jpeg_signature_orientation_is_applied_before_preview_and_embedding() {
        let mut jpeg = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(60, 20, image::Rgb([20, 20, 20])).write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        let jpeg = jpeg.into_inner();
        // Synthetic EXIF IFD with Orientation=6 (90 degrees clockwise).
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut rotated = jpeg[..2].to_vec();
        rotated.extend_from_slice(&[0xff, 0xe1]);
        rotated.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        rotated.extend_from_slice(exif);
        rotated.extend_from_slice(&jpeg[2..]);
        let image = SignatureImage::from_bytes(&rotated).unwrap();
        assert_eq!(image.size(), [20, 60]);
        assert_eq!(SignatureImage::from_bytes(image.bytes()).unwrap().size(), [20, 60]);
        assert!(!image.bytes().windows(4).any(|w| w == b"Exif"));
    }

    #[test]
    fn very_wide_image_signatures_keep_the_requested_rectangle() {
        let image = SignatureImage::from_bytes(&png(3000, 10)).unwrap();
        let mut session = crate::Session::new();
        let blank = session.create_blank(300.0, 400.0, 1).unwrap();
        let id = session.open_new("form.pdf", blank).unwrap();
        session.apply(id, image.edit(0, &page(0), [20.0, 100.0], false, "Ada").unwrap()).unwrap();
        let a = &session.get(id).unwrap().info.annotations[0];
        assert_eq!(a.rect, [20.0, 99.75, 170.0, 100.25]);
    }

    #[test]
    fn embedded_signature_preview_preserves_alpha_without_editing_the_document() {
        let image = SignatureImage::from_bytes(&png(120, 40)).unwrap();
        let mut session = crate::Session::new();
        let blank = session.create_blank(300.0, 400.0, 1).unwrap();
        let id = session.open_new("form.pdf", blank).unwrap();
        session.apply(id, image.edit(0, &page(0), [20.0, 100.0], false, "Ada").unwrap()).unwrap();
        let doc = session.get(id).unwrap();
        let bytes = doc.bytes.clone();
        let generation = doc.edit_generation();
        let preview = doc.image_signature_preview(0, 0).unwrap().unwrap();
        assert_eq!(preview.image.rgba(), image.rgba());
        let background = preview.render_background(1.0).unwrap();
        assert!(background.rgba.as_chunks::<4>().0.iter().all(|p| *p == [255, 255, 255, 255]));
        assert!(Arc::ptr_eq(&bytes, &doc.bytes));
        assert_eq!(doc.edit_generation(), generation);
        assert_eq!(doc.can_undo(), Some("Add signature"));
        assert!(doc.image_signature_preview(0, 1).is_err());
        assert!(preview.render_background(0.0).is_err());
    }

    #[test]
    fn oversized_embedded_signature_preview_is_rejected_before_decoding() {
        let image = SignatureImage::from_bytes(&png(120, 40)).unwrap();
        let mut session = crate::Session::new();
        let blank = session.create_blank(300.0, 400.0, 1).unwrap();
        let id = session.open_new("form.pdf", blank).unwrap();
        session.apply(id, image.edit(0, &page(0), [20.0, 100.0], false, "Ada").unwrap()).unwrap();
        let doc = session.doc_mut(id).unwrap();
        let cos = &mut doc.editor.as_mut().unwrap().cos;
        let picture = pdfcraft_annot::signature_image(cos, 0, 0).unwrap().unwrap();
        let pdfcraft_cos::Object::Stream(mut stream) = (*cos.get(picture)).clone() else { panic!() };
        stream.dict.set(b"Width".to_vec(), pdfcraft_cos::Object::Int(i64::MAX));
        cos.set(picture, pdfcraft_cos::Object::Stream(stream));
        let error = doc.image_signature_preview(0, 0).err().unwrap();
        assert!(error.contains("4 megapixels"), "{error}");
    }

    /// "Left edge at the point, centered vertically" is as displayed: on a turned page the box
    /// is another one in user space. The image is 64 x 32 pt; the displayed point is (60, 100).
    #[test]
    fn the_signature_box_is_laid_out_as_displayed_on_turned_pages() {
        let image = SignatureImage::from_bytes(&png(80, 40)).unwrap();
        for (rotation, at, want) in [
            (0, [60.0, 300.0], [60.0, 284.0, 124.0, 316.0]),
            (90, [100.0, 60.0], [84.0, 60.0, 116.0, 124.0]),
            (180, [240.0, 100.0], [176.0, 84.0, 240.0, 116.0]),
            (270, [200.0, 340.0], [184.0, 276.0, 216.0, 340.0]),
        ] {
            let rect = image.rect(&page(rotation), at, false).unwrap();
            assert!(rect.iter().zip(want).all(|(a, b)| (a - b).abs() < 0.01), "{rotation}: {rect:?}, want {want:?}");
        }
    }

    /// A synthetic photo: paper lit from `lit` (top-left) to `dark` (bottom-right) with a little
    /// deterministic noise, and a pen stroke `ink` thick across the middle third.
    fn photo(w: u32, h: u32, lit: f32, dark: f32, ink: [u8; 3]) -> image::RgbImage {
        let mut seed = 0x2545_f491u32;
        image::RgbImage::from_fn(w, h, |x, y| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed % 9) as f32 - 4.0;
            let t = (x as f32 / w as f32 + y as f32 / h as f32) / 2.0;
            let level = lit + (dark - lit) * t;
            let on_stroke = x > w / 3 && x < w * 2 / 3 && y.abs_diff(h / 2) < (h / 60).max(2);
            let px = |c: u8| ((if on_stroke { f32::from(c) * level / 255.0 } else { level }) + noise).clamp(0.0, 255.0) as u8;
            image::Rgb([px(ink[0]), px(ink[1]), px(ink[2])])
        })
    }

    fn encoded(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.into().write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_photo_on_grey_unevenly_lit_paper_becomes_ink_on_a_clear_background() {
        // Paper from 205 down to 140: none of it is "near white", and a fixed cutoff keeps it all.
        let source = encoded(photo(900, 600, 205.0, 140.0, [28, 36, 110]), ImageFormat::Jpeg);
        let kept = SignatureImage::from_bytes(&source).unwrap().remove_white_background(245, 35).unwrap();
        assert!(kept.rgba().as_chunks::<4>().0.iter().all(|p| p[3] == 255), "the fixed cutoff leaves a grey box");

        let image = SignatureImage::from_picture(&source).unwrap();
        let [w, h] = image.size();
        // Cropped to the stroke (300 x 19 in the photo) and its margin.
        assert!((300..=360).contains(&w) && (15..=80).contains(&h), "{w} x {h}");
        let pixels = image.rgba().as_chunks::<4>().0;
        let at = |x: usize, y: usize| pixels[y * w + x];
        assert_eq!(at(1, 1)[3], 0, "paper is clear");
        assert_eq!(at(w - 2, h - 2)[3], 0, "and so is the darker paper at the other end");
        let ink = at(w / 2, h / 2);
        assert_eq!(ink[3], 255, "ink is solid");
        assert!(ink[2] > ink[0] + 40, "and keeps its colour: {ink:?}");
        let clear = pixels.iter().filter(|p| p[3] == 0).count();
        let solid = pixels.iter().filter(|p| p[3] == 255).count();
        assert!(clear + solid > pixels.len() * 97 / 100, "no haze of half-clear paper: {clear} clear, {solid} solid of {}", pixels.len());
        assert!(pixels.iter().filter(|p| p[3] == 0).all(|p| p[..3] == [255, 255, 255]), "clear pixels hold no paper colour");
    }

    #[test]
    fn a_shadow_and_the_desk_around_the_sheet_are_not_taken_for_ink() {
        // The sheet ends at x = 660 with a dark desk beyond it; the left 300 columns are under a
        // shadow that halves the light, its edge crossing the stroke (x 240..480, y 290..310).
        let mut seed = 0x9e37_79b9u32;
        let photo = image::RgbImage::from_fn(800, 600, |x, y| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed % 7) as f32 - 3.0;
            let level = if x >= 660 {
                45.0
            } else if x < 300 {
                105.0
            } else {
                210.0
            };
            let on_stroke = (240..480).contains(&x) && (290..310).contains(&y);
            let v = (if on_stroke { level * 0.12 } else { level } + noise).clamp(0.0, 255.0) as u8;
            image::Rgb([v, v, v])
        });
        let image = SignatureImage::from_picture(&encoded(photo, ImageFormat::Png)).unwrap();
        let [w, h] = image.size();
        // Cropped to the stroke and its 18-pixel margin: the desk and the sheet's edge are gone.
        assert!((240..=290).contains(&w) && (20..=70).contains(&h), "{w} x {h}");
        let pixels = image.rgba().as_chunks::<4>().0;
        let row = |y: usize| &pixels[y * w..(y + 1) * w];
        assert!(row(h / 2)[20..w - 20].iter().all(|p| p[3] == 255), "the stroke is solid on both sides of the shadow's edge");
        assert!(row(h / 2)[20..w - 20].iter().all(|p| p[0] < 80), "and dark on both sides: lit as on white paper");
        assert!(row(2).iter().chain(row(h - 3)).all(|p| p[3] == 0), "paper above and below it is clear, in the shadow and out of it");
    }

    #[test]
    fn a_phone_sized_photo_is_accepted_and_shrunk() {
        // 12 megapixels: three times the stored limit.
        let source = encoded(photo(4000, 3000, 200.0, 170.0, [20, 20, 24]), ImageFormat::Jpeg);
        assert!(matches!(SignatureImage::from_bytes(&source), Err(SignatureImageError::Size)));
        let image = SignatureImage::from_picture(&source).unwrap();
        let [w, h] = image.size();
        // The stroke is a third of the 2000 pixels the photo is shrunk to.
        assert!((640..=760).contains(&w) && h < 200, "{w} x {h}");
        assert!(image.bytes().len() < 200_000, "{} bytes", image.bytes().len());
        assert_eq!(SignatureImage::from_bytes(image.bytes()).unwrap(), image, "what is stored reads back");
    }

    #[test]
    fn a_clean_scan_is_cleared_as_the_fixed_cutoff_clears_it() {
        let mut scan = image::RgbaImage::from_pixel(240, 80, image::Rgba([255, 255, 255, 255]));
        for x in 60..180 {
            for y in 36..44 {
                scan.put_pixel(x, y, image::Rgba([20, 20, 20, 255]));
            }
            // A soft edge: light grey, part ink.
            scan.put_pixel(x, 35, image::Rgba([227, 227, 227, 255]));
        }
        let source = encoded(scan, ImageFormat::Png);
        let fixed = SignatureImage::from_bytes(&source).unwrap().remove_white_background(245, 35).unwrap();
        let image = SignatureImage::from_picture(&source).unwrap();
        let [w, h] = image.size();
        assert!(w < 140 && h < 30, "cropped to the stroke: {w} x {h}");
        let pixels = image.rgba().as_chunks::<4>().0;
        let (left, top) = ((240 - w) / 2, 35 - (h - 9) / 2);
        for (i, p) in pixels.iter().enumerate() {
            let (x, y) = (i % w + left, i / w + top);
            let want = fixed.rgba().as_chunks::<4>().0[y * 240 + x];
            assert_eq!(p[3], want[3], "alpha at {x},{y}");
            if p[3] > 0 {
                assert_eq!(p[..3], want[..3], "ink is unchanged at {x},{y}");
            }
        }
    }

    #[test]
    fn a_prepared_cut_out_keeps_its_ink_and_is_only_cropped() {
        let mut cut = image::RgbaImage::from_pixel(200, 100, image::Rgba([0, 0, 0, 0]));
        for x in 50..150 {
            for y in 40..60 {
                // Pale ink that a paper estimate would take for the background.
                cut.put_pixel(x, y, image::Rgba([180, 190, 230, 200]));
            }
        }
        let image = SignatureImage::from_picture(&encoded(cut, ImageFormat::Png)).unwrap();
        let [w, h] = image.size();
        assert!((100..=112).contains(&w) && (20..=32).contains(&h), "{w} x {h}");
        assert_eq!(image.rgba().as_chunks::<4>().0[(h / 2) * w + w / 2], [180, 190, 230, 200]);
    }

    #[test]
    fn pictures_that_are_not_ink_on_paper_are_not_hollowed_out() {
        // A dark picture has no paper to remove; a blank sheet has no ink to crop to.
        let dark = SignatureImage::from_picture(&encoded(image::RgbImage::from_pixel(120, 60, image::Rgb([30, 40, 50])), ImageFormat::Png)).unwrap();
        assert_eq!(dark.size(), [120, 60]);
        assert!(dark.rgba().as_chunks::<4>().0.iter().all(|p| *p == [30, 40, 50, 255]));
        let blank =
            SignatureImage::from_picture(&encoded(image::RgbImage::from_pixel(120, 60, image::Rgb([200, 198, 190])), ImageFormat::Png)).unwrap();
        assert_eq!(blank.size(), [120, 60]);
        assert!(blank.rgba().as_chunks::<4>().0.iter().all(|p| p[3] == 0));
        // A marker stroke thicker than the grid the paper level is measured on stays solid inside.
        let mut marker = image::RgbImage::from_pixel(400, 300, image::Rgb([190, 190, 185]));
        for x in 60..340 {
            for y in 90..210 {
                marker.put_pixel(x, y, image::Rgb([25, 25, 30]));
            }
        }
        let marker = SignatureImage::from_picture(&encoded(marker, ImageFormat::Png)).unwrap();
        let [w, h] = marker.size();
        assert_eq!(marker.rgba().as_chunks::<4>().0[(h / 2) * w + w / 2][3], 255);
    }

    #[test]
    fn oversized_and_invalid_pictures_are_rejected() {
        assert!(matches!(SignatureImage::from_picture(b"GIF89a\x02\0\x02\0"), Err(SignatureImageError::Format)));
        assert!(SignatureImage::from_picture(b"\x89PNG\r\n\x1a\ntruncated").is_err());
        assert!(matches!(SignatureImage::from_picture(&vec![0; MAX_PICTURE_BYTES + 1]), Err(SignatureImageError::PictureSize)));
        assert!(matches!(SignatureImage::from_picture(&png(8193, 1)), Err(SignatureImageError::PictureSize)));
        assert!(SignatureImage::read_picture(std::io::repeat(0)).is_err());
        // A PNG header claiming 8192 x 8192 (67 megapixels) is refused before any pixel is read.
        let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        huge.extend_from_slice(&[0, 0, 0x20, 0, 0, 0, 0x20, 0, 8, 2, 0, 0, 0]);
        huge.extend_from_slice(&[0xfd, 0xc8, 0x5d, 0x0e]);
        // The start of its image data, which is as far as the header is read.
        huge.extend_from_slice(&[0, 0, 0, 2, 73, 68, 65, 84, 120, 156, 98, 164, 145, 43]);
        assert!(
            matches!(SignatureImage::from_picture(&huge), Err(SignatureImageError::PictureSize)),
            "{:?}",
            SignatureImage::from_picture(&huge).err()
        );
    }

    #[test]
    fn white_background_becomes_transparent_and_ink_is_kept() {
        let mut source = image::RgbaImage::new(4, 1);
        source.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));
        source.put_pixel(1, 0, image::Rgba([20, 20, 20, 255]));
        source.put_pixel(2, 0, image::Rgba([227, 227, 227, 255]));
        source.put_pixel(3, 0, image::Rgba([10, 10, 10, 100]));
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, ImageFormat::Png).unwrap();

        let image = SignatureImage::from_bytes(&encoded.into_inner()).unwrap();
        let cleaned = image.remove_white_background(245, 35).unwrap();
        let pixels = cleaned.rgba().as_chunks::<4>().0;

        assert_eq!(cleaned.size(), [4, 1]);
        assert_eq!(pixels[0][3], 0, "white paper must be transparent");
        assert_eq!(pixels[1], [20, 20, 20, 255], "dark ink must be unchanged");
        assert!(pixels[2][3] > 0 && pixels[2][3] < 255, "near-white must fade");
        assert_eq!(pixels[3][3], 100, "existing transparency must be kept");
    }
}
