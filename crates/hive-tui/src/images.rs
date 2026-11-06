//! Bounded PNG layout and Kitty output, independent of ACP content models.
//!
//! Callers allocate transcript rows before calling [`ImageFrame::render`]. A frame
//! records desired placements; only the terminal owner writes graphics commands.

use std::{
    collections::BTreeMap,
    io,
    io::Write,
    sync::{Arc, OnceLock},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::{cursor, queue};
use ratatui::{Frame, layout::Rect, widgets::Paragraph};
use uuid::Uuid;

use crate::capabilities::TerminalCapabilities;

// Bound both transfer cost and terminal-side allocations before accepting a source.
const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PIXELS: u64 = 40_000_000;
pub(crate) const MAX_DIMENSION: u32 = 16_384;
const CHUNK_BYTES: usize = 4096;

/// Encoded message data stays independent of terminal capabilities and survives reflow.
#[derive(Debug)]
pub struct EncodedImage {
    // Keep payload and metadata immutable once preparation has established an upload identity.
    data: String,
    mime_type: String,
    uri: Option<String>,
    dimensions: OnceLock<Result<(u32, u32), PrepareError>>,
    prepared: OnceLock<Result<ImageSource, PrepareError>>,
}

impl PartialEq for EncodedImage {
    fn eq(&self, other: &Self) -> bool {
        // Cache population is a rendering detail, not a change to conversation content.
        self.data == other.data && self.mime_type == other.mime_type && self.uri == other.uri
    }
}

impl Eq for EncodedImage {}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("unsupported image format {0}; provide PNG, JPEG, or WebP")]
    Unsupported(String),
    #[error("invalid base64 image data; provide base64-encoded image bytes")]
    Base64,
    #[error(transparent)]
    Source(#[from] ImageError),
    #[error("could not decode or convert image: {0}; provide a valid, smaller PNG, JPEG, or WebP")]
    Decode(#[from] image::ImageError),
}

impl EncodedImage {
    pub(crate) fn attachment(
        &self,
    ) -> Result<Arc<crate::draft::Attachment>, crate::draft::DraftError> {
        if self.data.len() > crate::draft::MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(crate::draft::DraftError::Limit);
        }
        crate::draft::Attachment::new(self.mime_type.clone(), self.bytes()?)
    }
    pub fn new(data: String, mime_type: String, uri: Option<String>) -> Self {
        Self {
            data,
            mime_type,
            uri,
            dimensions: OnceLock::new(),
            prepared: OnceLock::new(),
        }
    }

    pub fn source(&self) -> Result<&ImageSource, &PrepareError> {
        self.dimensions()?;
        // Retain failures too: malformed agent data must not be decoded on every caret tick.
        self.prepared.get_or_init(|| self.prepare()).as_ref()
    }

    pub fn dimensions(&self) -> Result<(u32, u32), &PrepareError> {
        self.dimensions
            .get_or_init(|| self.inspect())
            .as_ref()
            .copied()
    }

    pub(crate) fn failed(&self) -> bool {
        self.prepared.get().is_some_and(Result::is_err)
    }

    #[cfg(test)]
    pub(crate) fn is_prepared(&self) -> bool {
        self.prepared.get().is_some()
    }

    pub fn caption(&self, ordinal: usize) -> String {
        // Fallback labels and off-screen layout need headers, never a full pixel conversion.
        let details = match self.prepared.get() {
            Some(Err(error)) => error.to_string(),
            _ => match self.dimensions() {
                Ok((width, height)) => format!("{width}×{height}"),
                Err(error) => error.to_string(),
            },
        };
        let uri = self.uri.as_deref().unwrap_or_default();
        // Agent-provided metadata must remain one logical caption, even with control characters.
        format!("[image {ordinal}: {}] {details} {uri}", self.mime_type)
            .chars()
            .filter(|ch| !ch.is_control() || ch.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn format(&self) -> Result<image::ImageFormat, PrepareError> {
        match self.mime_type.as_str() {
            "image/png" => Ok(image::ImageFormat::Png),
            "image/jpeg" => Ok(image::ImageFormat::Jpeg),
            "image/webp" => Ok(image::ImageFormat::WebP),
            other => Err(PrepareError::Unsupported(other.to_owned())),
        }
    }

    fn bytes(&self) -> Result<Vec<u8>, PrepareError> {
        // Bound allocation before decoding base64, not only after creating a byte vector.
        if self.data.len() > MAX_BYTES.div_ceil(3) * 4 {
            return Err(ImageError::TooLarge.into());
        }
        let bytes = STANDARD
            .decode(&self.data)
            .map_err(|_| PrepareError::Base64)?;
        if bytes.len() > MAX_BYTES {
            return Err(ImageError::TooLarge.into());
        }
        Ok(bytes)
    }

    fn reader(
        bytes: Vec<u8>,
        format: image::ImageFormat,
    ) -> image::ImageReader<io::Cursor<Vec<u8>>> {
        let mut reader = image::ImageReader::with_format(io::Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        limits.max_alloc = Some(MAX_PIXELS * 4);
        reader.limits(limits);
        reader
    }

    fn inspect(&self) -> Result<(u32, u32), PrepareError> {
        let format = self.format()?;
        let bytes = self.bytes()?;
        let (width, height) = if format == image::ImageFormat::Png {
            ImageSource::png(bytes)?.dimensions()
        } else {
            let decoder = Self::reader(bytes, format).into_decoder()?;
            image::ImageDecoder::dimensions(&decoder)
        };
        // The pixel-count limit is stricter than the per-dimension decoder limit.
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(ImageError::Dimensions { width, height }.into());
        }
        Ok((width, height))
    }

    fn prepare(&self) -> Result<ImageSource, PrepareError> {
        let format = self.format()?;
        let bytes = self.bytes()?;
        if format == image::ImageFormat::Png {
            return Ok(ImageSource::png(bytes)?);
        }
        let decoder = Self::reader(bytes, format).into_decoder()?;
        let (width, height) = image::ImageDecoder::dimensions(&decoder);
        let decoded = image::DynamicImage::from_decoder(decoder)?.into_rgba8();
        let mut output = BoundedPng(Vec::new());
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut output),
            &decoded,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(ImageSource::png(output.0)?)
    }
}

// PNG output can exceed compressed input; stop the encoder before an oversized allocation.
struct BoundedPng(Vec<u8>);

impl Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other(
                "converted PNG exceeds the 16 MiB transfer limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A rejected source can still be represented by its caller's textual metadata.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error(
        "image is not a PNG with a complete IHDR header; provide PNG data or display its metadata"
    )]
    InvalidPng,
    #[error("image exceeds the {MAX_BYTES}-byte transfer limit; provide a smaller image")]
    TooLarge,
    #[error(
        "image dimensions {width}×{height} exceed supported bounds; resize the image before displaying it"
    )]
    Dimensions { width: u32, height: u32 },
}

/// Immutable PNG bytes; cloning preserves upload identity without copying pixels.
#[derive(Clone, Debug)]
pub struct ImageSource {
    key: Uuid,
    png: Arc<[u8]>,
    width: u32,
    height: u32,
}

impl ImageSource {
    /// Check the transport/header bounds, not the complete PNG compressed stream.
    /// The terminal decodes PNG; callers retain captions even when decoding fails.
    pub fn png(bytes: impl Into<Arc<[u8]>>) -> Result<Self, ImageError> {
        let png = bytes.into();
        if png.len() > MAX_BYTES {
            return Err(ImageError::TooLarge);
        }
        if png.len() < 33
            || &png[..8] != b"\x89PNG\r\n\x1a\n"
            || png[8..12] != 13_u32.to_be_bytes()
            || &png[12..16] != b"IHDR"
        {
            return Err(ImageError::InvalidPng);
        }
        let width = u32::from_be_bytes(png[16..20].try_into().expect("checked header length"));
        let height = u32::from_be_bytes(png[20..24].try_into().expect("checked header length"));
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_PIXELS
        {
            return Err(ImageError::Dimensions { width, height });
        }
        Ok(Self {
            key: Uuid::new_v4(),
            png,
            width,
            height,
        })
    }

    /// Pixel dimensions used in fallback metadata, without terminal-specific units.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Cell bounds; Kitty scales without distorting the source inside this rectangle.
#[derive(Clone, Copy, Debug)]
pub struct ImageBounds {
    pub columns: u16,
    pub rows: u16,
}

impl ImageBounds {
    pub const INLINE: Self = Self {
        columns: 80,
        rows: 20,
    };
    pub const THUMBNAIL: Self = Self {
        columns: 20,
        rows: 6,
    };
}

struct Placement {
    source: ImageSource,
    area: Rect,
}

/// Desired graphics for one draw. Dropping an old frame never writes to a terminal.
pub struct ImageFrame {
    enabled: bool,
    placements: Vec<Placement>,
}

impl ImageFrame {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub(crate) fn areas(&self) -> Vec<Rect> {
        self.placements
            .iter()
            .map(|placement| placement.area)
            .collect()
    }

    /// Captions participate in transcript selection; only reserve pixels here.
    pub(crate) fn place(&mut self, source: &ImageSource, area: Rect) {
        if self.enabled && !area.is_empty() {
            self.placements.push(Placement {
                source: source.clone(),
                area,
            });
        }
    }

    pub fn new(capabilities: &TerminalCapabilities) -> Self {
        Self {
            enabled: capabilities.kitty_graphics.is_usable(),
            placements: Vec::new(),
        }
    }

    /// Render a caption and reserve image rows within an already allocated area.
    /// Returns the consumed height, including the caption. `None` renders fallback
    /// metadata only, allowing callers to handle invalid/unsupported image formats.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        source: Option<&ImageSource>,
        label: &str,
        area: Rect,
        bounds: ImageBounds,
    ) -> u16 {
        let area = area.intersection(frame.area());
        if area.is_empty() {
            return 0;
        }
        let caption = source.map_or_else(
            || format!("[image] {label}"),
            |source| format!("[image/png {}×{}] {label}", source.width, source.height),
        );
        frame.render_widget(Paragraph::new(caption), Rect { height: 1, ..area });
        let image_area = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width.min(bounds.columns),
            area.height.saturating_sub(1).min(bounds.rows),
        );
        if self.enabled
            && !image_area.is_empty()
            && let Some(source) = source
        {
            // Clear reserved cells in the text buffer, so previous content cannot show through.
            frame.render_widget(ratatui::widgets::Clear, image_area);
            self.placements.push(Placement {
                source: source.clone(),
                area: image_area,
            });
            return 1 + image_area.height;
        }
        1
    }

    /// Hide graphics behind an overlay whose text clearing cannot erase Kitty images.
    pub fn suppress(&mut self) {
        self.placements.clear();
    }
}

struct Resident {
    id: u32,
    uploaded: bool,
}

/// Owned by the terminal guard: uploads must not outlive alternate-screen ownership.
pub(crate) struct KittyRenderer {
    sources: BTreeMap<Uuid, Resident>,
    next_id: u32,
    invalidated: bool,
    size: Option<ratatui::layout::Size>,
}

impl Default for KittyRenderer {
    fn default() -> Self {
        // There is no application namespace in Kitty; randomize the starting ID to
        // avoid colliding with other terminal clients, never use the implicit ID 0.
        let random = Uuid::new_v4();
        let seed = u32::from_be_bytes(random.as_bytes()[..4].try_into().unwrap());
        Self {
            sources: BTreeMap::new(),
            next_id: seed.max(1),
            invalidated: false,
            size: None,
        }
    }
}

impl KittyRenderer {
    pub(crate) fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub(crate) fn resize(&mut self, size: ratatui::layout::Size) {
        if self.size != Some(size) {
            self.invalidate();
            self.size = Some(size);
        }
    }

    /// Remove placements before text redraws. Retain uploads unless the screen was reset.
    pub(crate) fn begin(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.invalidated {
            self.cleanup(writer)?;
            self.invalidated = false;
        } else {
            for resident in self.sources.values() {
                delete(writer, resident.id, false)?;
            }
            writer.flush()?;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, writer: &mut impl Write, frame: ImageFrame) -> io::Result<()> {
        // Retry from a known screen state after any partial write/flush failure.
        let result = self.finish_inner(writer, frame);
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn finish_inner(&mut self, writer: &mut impl Write, frame: ImageFrame) -> io::Result<()> {
        if self.invalidated {
            // Drawing may have resized the screen since our pre-draw geometry check.
            self.cleanup(writer)?;
            self.invalidated = false;
        }
        let removed: Vec<_> = self
            .sources
            .keys()
            .copied()
            .filter(|key| {
                !frame
                    .placements
                    .iter()
                    .any(|placement| placement.source.key == *key)
            })
            .collect();
        for key in removed {
            delete(writer, self.sources[&key].id, true)?;
            writer.flush()?;
            self.sources.remove(&key);
        }
        for placement in &frame.placements {
            if !self.sources.contains_key(&placement.source.key) {
                // IDs are never reused during a terminal lifetime, including after failures.
                let id = self.next_id;
                if id == 0 {
                    return Err(io::Error::other(
                        "Kitty image IDs exhausted; restart the TUI",
                    ));
                }
                self.next_id = id.checked_add(1).unwrap_or(0);
                // Register before upload so cleanup includes partially transmitted images.
                self.sources.insert(
                    placement.source.key,
                    Resident {
                        id,
                        uploaded: false,
                    },
                );
            }
            let resident = self.sources.get_mut(&placement.source.key).unwrap();
            if !resident.uploaded {
                upload(writer, resident.id, &placement.source.png)?;
                writer.flush()?;
                resident.uploaded = true;
            }
        }
        if frame.placements.is_empty() {
            return writer.flush();
        }
        queue!(writer, cursor::SavePosition)?;
        let result = (|| {
            for (index, placement) in frame.placements.iter().enumerate() {
                let id = self.sources[&placement.source.key].id;
                let occurrence = u32::try_from(index + 1).map_err(|_| {
                    io::Error::other("too many image placements; show fewer images")
                })?;
                queue!(writer, cursor::MoveTo(placement.area.x, placement.area.y))?;
                write!(
                    writer,
                    "\x1b_Ga=p,i={id},p={occurrence},c={},r={},C=1,q=2\x1b\\",
                    placement.area.width, placement.area.height
                )?;
            }
            Ok(())
        })();
        // C=1 suppresses Kitty's movement, not our explicit MoveTo above.
        let restore = queue!(writer, cursor::RestorePosition).and_then(|()| writer.flush());
        result.and(restore)
    }

    pub(crate) fn cleanup(&mut self, writer: &mut impl Write) -> io::Result<()> {
        // Keep all IDs until the flush succeeds, so the guard can retry on failure.
        for resident in self.sources.values() {
            delete(writer, resident.id, true)?;
        }
        if !self.sources.is_empty() {
            writer.flush()?;
        }
        self.sources.clear();
        Ok(())
    }
}

fn delete(writer: &mut impl Write, id: u32, data: bool) -> io::Result<()> {
    let selector = if data { 'I' } else { 'i' };
    write!(writer, "\x1b_Ga=d,d={selector},i={id},q=2\x1b\\")
}

fn upload(writer: &mut impl Write, id: u32, png: &[u8]) -> io::Result<()> {
    let encoded = STANDARD.encode(png);
    let chunks = encoded.as_bytes().chunks(CHUNK_BYTES);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let more = u8::from(index + 1 < count);
        if index == 0 {
            write!(writer, "\x1b_Ga=t,t=d,f=100,i={id},q=2,m={more};")?;
        } else {
            // Continuations must not repeat image IDs or format/transport parameters.
            write!(writer, "\x1b_Gm={more},q=2;")?;
        }
        writer.write_all(chunk)?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fixture_source() -> ImageSource {
    // An opaque blue/orange checkerboard makes letterboxing and reflow visible.
    ImageSource::png(STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAGUlEQVR4nGPQWPD/f4AGJsmAVRRIMgxKHQCKIGfBlA0fRgAAAABJRU5ErkJggg==").unwrap()).unwrap()
}

#[cfg(test)]
pub(crate) fn fixture_block() -> serde_json::Value {
    serde_json::json!({"type":"image", "mimeType":"image/png", "data": STANDARD.encode(&fixture_source().png)})
}

#[cfg(test)]
pub(crate) fn second_fixture_block() -> serde_json::Value {
    let pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([200, 20, 40, 255]));
    let mut bytes = io::Cursor::new(Vec::new());
    pixels
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    serde_json::json!({"type":"image", "mimeType":"image/png", "data":STANDARD.encode(bytes.into_inner())})
}

#[cfg(test)]
pub(crate) fn fixture_scene(frame: &mut Frame, images: &mut ImageFrame, source: &ImageSource) {
    let area = frame.area();
    frame.render_widget(
        Paragraph::new("Kitty fixture: text before image"),
        Rect::new(0, 0, area.width, area.height.min(1)),
    );
    let body = Rect::new(0, 1, area.width, area.height.saturating_sub(1));
    let consumed = images.render(
        frame,
        Some(source),
        "inline checkerboard",
        body,
        ImageBounds {
            columns: 60,
            rows: 8,
        },
    );
    let y = 1 + consumed;
    if y < area.height {
        frame.render_widget(
            Paragraph::new("Text after image; thumbnail below"),
            Rect::new(0, y, area.width, 1),
        );
        images.render(
            frame,
            Some(source),
            "thumbnail",
            Rect::new(0, y + 1, area.width, area.height.saturating_sub(y + 1)),
            ImageBounds::THUMBNAIL,
        );
    }
    frame.set_cursor_position((0, 0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    pub(super) fn source() -> ImageSource {
        fixture_source()
    }

    #[test]
    fn common_formats_prepare_once_and_preserve_dimensions_and_pixels() {
        for (mime, format) in [
            ("image/png", image::ImageFormat::Png),
            ("image/jpeg", image::ImageFormat::Jpeg),
            ("image/webp", image::ImageFormat::WebP),
        ] {
            let pixels = image::RgbImage::from_pixel(3, 2, image::Rgb([20, 80, 150]));
            let mut bytes = io::Cursor::new(Vec::new());
            pixels.write_to(&mut bytes, format).unwrap();
            let encoded = EncodedImage::new(STANDARD.encode(bytes.into_inner()), mime.into(), None);
            let source = encoded.source().unwrap();
            assert_eq!(source.dimensions(), (3, 2));
            assert_eq!(source.key, encoded.source().unwrap().key);
            let decoded = image::load_from_memory(&source.png).unwrap().into_rgb8();
            for channel in 0..3 {
                // JPEG is lossy; PNG/WebP fixtures are lossless.
                assert!(decoded[(0, 0)][channel].abs_diff(pixels[(0, 0)][channel]) <= 3);
            }
            assert!(encoded.caption(2).contains(&format!("image 2: {mime}")));
        }
    }

    #[test]
    fn animated_webp_conversion_uses_the_first_frame_and_preserves_transparency() {
        fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
            output.extend_from_slice(kind);
            output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            output.extend_from_slice(bytes);
            if !bytes.len().is_multiple_of(2) {
                output.push(0);
            }
        }
        let mut first = image::RgbaImage::from_pixel(2, 1, image::Rgba([200, 20, 40, 255]));
        first[(1, 0)] = image::Rgba([0, 0, 0, 0]);
        let second = image::RgbaImage::from_pixel(2, 1, image::Rgba([10, 220, 30, 255]));
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        // A 2×1 animated alpha canvas; explicit container chunks distinguish frame selection.
        chunk(&mut webp, b"VP8X", &[0x12, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        chunk(&mut webp, b"ANIM", &[0; 6]);
        for frame in [&first, &second] {
            let mut encoded = io::Cursor::new(Vec::new());
            frame
                .write_to(&mut encoded, image::ImageFormat::WebP)
                .unwrap();
            // Origin 0,0, dimensions 2×1, 10 ms, replace rather than blend with the background.
            let mut payload = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 10, 0, 0, 2];
            payload.extend_from_slice(&encoded.into_inner()[12..]);
            chunk(&mut webp, b"ANMF", &payload);
        }
        let length = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&length.to_le_bytes());
        let encoded = EncodedImage::new(STANDARD.encode(webp), "image/webp".into(), None);
        assert_eq!(encoded.dimensions().unwrap(), (2, 1));
        assert!(!encoded.is_prepared());
        let converted = image::load_from_memory(&encoded.source().unwrap().png)
            .unwrap()
            .into_rgba8();
        assert_eq!(converted, first);
    }

    #[test]
    fn invalid_encoded_sources_cache_actionable_fallbacks_without_exposing_payloads() {
        for (mime, data, reason) in [
            (
                "image/gif",
                "secret-payload".into(),
                "unsupported image format",
            ),
            ("image/png", "secret-payload".into(), "invalid base64"),
            ("image/png", STANDARD.encode(b"secret-payload"), "not a PNG"),
            (
                "image/jpeg",
                STANDARD.encode(b"secret-payload"),
                "could not decode",
            ),
            (
                "image/webp",
                STANDARD.encode(b"secret-payload"),
                "could not decode",
            ),
        ] {
            let encoded = EncodedImage::new(data, mime.into(), Some("file:///image".into()));
            assert!(encoded.caption(1).contains(reason));
            assert!(encoded.caption(1).contains("file:///image"));
            assert!(!encoded.caption(1).contains("secret-payload"));
            assert!(std::ptr::eq(
                encoded.source().unwrap_err(),
                encoded.source().unwrap_err()
            ));
        }
    }

    #[test]
    fn encoded_input_and_conversion_output_are_bounded_before_growing_buffers() {
        let encoded = EncodedImage::new(
            "A".repeat(MAX_BYTES.div_ceil(3) * 4 + 1),
            "image/png".into(),
            None,
        );
        assert!(matches!(
            encoded.source(),
            Err(PrepareError::Source(ImageError::TooLarge))
        ));
        let mut output = BoundedPng(vec![0; MAX_BYTES - 1]);
        assert!(output.write_all(&[1, 2]).is_err());
        assert_eq!(output.0.len(), MAX_BYTES - 1);
        let mut bytes = fixture_source().png.to_vec();
        bytes[16..20].copy_from_slice(&(MAX_DIMENSION + 1).to_be_bytes());
        let encoded = EncodedImage::new(STANDARD.encode(bytes), "image/png".into(), None);
        assert!(matches!(
            encoded.source(),
            Err(PrepareError::Source(ImageError::Dimensions { .. }))
        ));
    }

    #[test]
    fn conversion_rejects_excessive_dimensions_before_decoding_pixels() {
        let pixels = image::RgbImage::from_pixel(1, 1, image::Rgb([0, 0, 0]));
        let mut jpeg = io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let original = jpeg.into_inner();
        let header = original
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        for (width, height) in [(MAX_DIMENSION + 1, 1), (8000, 8000)] {
            let mut bytes = original.clone();
            // Alter only SOF dimensions: a decoder must reject bounds before reading missing pixels.
            bytes[header + 5..header + 7].copy_from_slice(&(height as u16).to_be_bytes());
            bytes[header + 7..header + 9].copy_from_slice(&(width as u16).to_be_bytes());
            let encoded = EncodedImage::new(STANDARD.encode(bytes), "image/jpeg".into(), None);
            assert!(encoded.source().is_err());
            assert!(
                encoded.caption(1).contains("smaller") || encoded.caption(1).contains("resize")
            );
        }
    }

    fn scene(
        source: &ImageSource,
        enabled: bool,
        width: u16,
        height: u16,
    ) -> (ImageFrame, Terminal<TestBackend>) {
        let mut images = ImageFrame {
            enabled,
            placements: Vec::new(),
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new("before"), Rect::new(0, 0, area.width, 1));
                let consumed = images.render(
                    frame,
                    Some(source),
                    "inline",
                    Rect::new(0, 1, area.width, area.height.saturating_sub(2)),
                    ImageBounds::INLINE,
                );
                let y = 1 + consumed;
                if y < height {
                    frame.render_widget(Paragraph::new("after"), Rect::new(0, y, area.width, 1));
                }
            })
            .unwrap();
        (images, terminal)
    }

    #[test]
    fn png_headers_reject_truncation_zero_dimensions_and_excessive_resources() {
        assert!(matches!(
            ImageSource::png(vec![0; 32]),
            Err(ImageError::InvalidPng)
        ));
        assert!(matches!(
            ImageSource::png(vec![0; MAX_BYTES + 1]),
            Err(ImageError::TooLarge)
        ));
        let original = source();
        assert_eq!(original.dimensions(), (8, 8));
        for (width, height) in [(0, 1_u32), (1, 0), (MAX_DIMENSION + 1, 1), (10_000, 10_000)] {
            let mut bytes = original.png.to_vec();
            bytes[16..20].copy_from_slice(&width.to_be_bytes());
            bytes[20..24].copy_from_slice(&height.to_be_bytes());
            assert!(matches!(
                ImageSource::png(bytes),
                Err(ImageError::Dimensions { .. })
            ));
        }
    }

    #[test]
    fn direct_transfers_round_trip_and_continuations_only_carry_chunk_keys() {
        for bytes in [source().png.to_vec(), vec![42; 12_001]] {
            let mut output = Vec::new();
            upload(&mut output, 123, &bytes).unwrap();
            let output = String::from_utf8(output).unwrap();
            let commands: Vec<_> = output.split("\x1b_G").skip(1).collect();
            let mut encoded = String::new();
            for (index, command) in commands.iter().enumerate() {
                let (header, payload) = command
                    .strip_suffix("\x1b\\")
                    .unwrap()
                    .split_once(';')
                    .unwrap();
                assert!(payload.len() <= CHUNK_BYTES);
                let more = usize::from(index + 1 < commands.len());
                if more == 1 {
                    assert_eq!(payload.len() % 4, 0);
                }
                if index == 0 {
                    assert_eq!(header, format!("a=t,t=d,f=100,i=123,q=2,m={more}"));
                } else {
                    assert_eq!(header, format!("m={more},q=2"));
                }
                encoded.push_str(payload);
            }
            assert_eq!(STANDARD.decode(encoded).unwrap(), bytes);
        }
    }

    #[test]
    fn bounded_images_reserve_rows_between_text_and_share_one_upload() {
        let source = source();
        let (mut images, terminal) = scene(&source, true, 100, 40);
        assert_eq!(images.placements[0].area, Rect::new(0, 2, 80, 20));
        assert_eq!(terminal.backend().buffer()[(0, 22)].symbol(), "a");
        let mut thumbnail = ImageFrame {
            enabled: true,
            placements: Vec::new(),
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                thumbnail.render(
                    frame,
                    Some(&source),
                    "thumbnail",
                    Rect::new(5, 25, 40, 12),
                    ImageBounds::THUMBNAIL,
                );
            })
            .unwrap();
        assert_eq!(thumbnail.placements[0].area, Rect::new(5, 26, 20, 6));
        images.placements.extend(thumbnail.placements);
        let mut renderer = KittyRenderer::default();
        let mut bytes = Vec::new();
        renderer.finish(&mut bytes, images).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert_eq!(output.matches("a=t,").count(), 1);
        assert_eq!(output.matches("a=p,").count(), 2);
        assert!(output.contains("p=1,c=80,r=20,C=1,q=2"));
        assert!(output.contains("p=2,c=20,r=6,C=1,q=2"));
        assert!(output.contains("\x1b7"));
        assert!(output.ends_with("\x1b8"));
    }

    #[test]
    fn fallback_and_empty_viewports_never_emit_graphics() {
        let source = source();
        for (enabled, width, height) in [(false, 80, 24), (true, 0, 0), (true, 1, 1), (true, 1, 2)]
        {
            let (images, _) = scene(&source, enabled, width, height);
            assert!(images.placements.is_empty());
            let mut bytes = Vec::new();
            KittyRenderer::default().finish(&mut bytes, images).unwrap();
            assert!(bytes.is_empty());
        }
        let (images, terminal) = scene(&source, false, 80, 24);
        assert!(images.placements.is_empty());
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("[image/png 8×8] inline"));
        assert!(text.contains("after"));
    }

    #[test]
    fn unknown_and_disabled_capabilities_keep_metadata_without_graphics() {
        for support in [
            crate::capabilities::Support::Unknown,
            crate::capabilities::Support::Unsupported,
        ] {
            let mut capabilities = TerminalCapabilities::default();
            capabilities.kitty_graphics.effective = support;
            let mut images = ImageFrame::new(&capabilities);
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal
                .draw(|frame| {
                    assert_eq!(
                        images.render(
                            frame,
                            Some(&source()),
                            "diagram",
                            frame.area(),
                            ImageBounds::INLINE
                        ),
                        1
                    );
                    assert_eq!(
                        images.render(
                            frame,
                            None,
                            "diagram.jpg (unsupported format)",
                            Rect::new(0, 1, 40, 11),
                            ImageBounds::INLINE
                        ),
                        1
                    );
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("[image] diagram.jpg (unsupported format)"));
            let mut bytes = Vec::new();
            let mut renderer = KittyRenderer::default();
            renderer.begin(&mut bytes).unwrap();
            renderer.finish(&mut bytes, images).unwrap();
            renderer.cleanup(&mut bytes).unwrap();
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn placements_are_clamped_to_the_frame_and_zero_bounds_reserve_no_image_rows() {
        let mut images = ImageFrame {
            enabled: true,
            placements: Vec::new(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                assert_eq!(
                    images.render(
                        frame,
                        Some(&source()),
                        "clipped",
                        Rect::new(35, 8, 40, 20),
                        ImageBounds::INLINE
                    ),
                    4
                );
                assert_eq!(
                    images.render(
                        frame,
                        Some(&source()),
                        "zero bounds",
                        frame.area(),
                        ImageBounds {
                            columns: 0,
                            rows: 20
                        }
                    ),
                    1
                );
            })
            .unwrap();
        assert_eq!(images.placements.len(), 1);
        assert_eq!(images.placements[0].area, Rect::new(35, 9, 5, 3));
    }

    #[test]
    fn removing_one_source_keeps_other_uploads_resident() {
        let first = source();
        let second = source();
        let mut images = scene(&first, true, 80, 24).0;
        images
            .placements
            .extend(scene(&second, true, 80, 24).0.placements);
        let mut renderer = KittyRenderer::default();
        let mut bytes = Vec::new();
        renderer.finish(&mut bytes, images).unwrap();
        let first_id = renderer.sources[&first.key].id;
        let second_id = renderer.sources[&second.key].id;
        bytes.clear();
        renderer.begin(&mut bytes).unwrap();
        renderer
            .finish(&mut bytes, scene(&second, true, 80, 24).0)
            .unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains(&format!("d=I,i={first_id},")));
        assert!(!output.contains(&format!("d=I,i={second_id},")));
        assert!(!output.contains("a=t,"));
        assert_eq!(renderer.sources.len(), 1);
    }

    #[test]
    fn redraws_reuse_uploads_and_removal_frees_only_owned_image_ids() {
        let source = source();
        let mut renderer = KittyRenderer::default();
        let mut bytes = Vec::new();
        renderer
            .finish(&mut bytes, scene(&source, true, 80, 24).0)
            .unwrap();
        let id = renderer.sources[&source.key].id;
        bytes.clear();
        renderer.begin(&mut bytes).unwrap();
        renderer
            .finish(&mut bytes, scene(&source, true, 40, 12).0)
            .unwrap();
        let output = String::from_utf8(bytes.clone()).unwrap();
        assert!(output.starts_with(&format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\")));
        assert!(!output.contains("a=t,"));
        assert!(output.contains("c=40,r=9"));
        bytes.clear();
        renderer
            .finish(
                &mut bytes,
                ImageFrame {
                    enabled: true,
                    placements: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
        );
        assert!(renderer.sources.is_empty());
    }

    #[test]
    fn screen_invalidation_reuploads_and_overlays_remove_old_images() {
        let source = source();
        let mut renderer = KittyRenderer::default();
        let mut bytes = Vec::new();
        renderer
            .finish(&mut bytes, scene(&source, true, 80, 24).0)
            .unwrap();
        renderer.invalidate();
        bytes.clear();
        renderer.begin(&mut bytes).unwrap();
        renderer
            .finish(&mut bytes, scene(&source, true, 80, 24).0)
            .unwrap();
        let output = String::from_utf8(bytes.clone()).unwrap();
        assert!(output.contains("d=I,"));
        assert!(output.contains("a=t,"));
        let mut images = scene(&source, true, 80, 24).0;
        images.suppress();
        bytes.clear();
        renderer.begin(&mut bytes).unwrap();
        renderer.finish(&mut bytes, images).unwrap();
        assert!(renderer.sources.is_empty());
        assert!(!String::from_utf8(bytes).unwrap().contains("a=p,"));
    }

    #[test]
    fn failed_uploads_and_cleanup_keep_ids_available_for_retry() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("fixture flush failure"))
            }
        }
        let source = source();
        let mut renderer = KittyRenderer::default();
        assert!(
            renderer
                .finish(&mut Failing, scene(&source, true, 80, 24).0)
                .is_err()
        );
        let id = renderer.sources[&source.key].id;
        assert!(!renderer.sources[&source.key].uploaded);
        assert!(renderer.cleanup(&mut Failing).is_err());
        assert_eq!(renderer.sources[&source.key].id, id);
        let mut bytes = Vec::new();
        renderer.begin(&mut bytes).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
        );
        assert!(renderer.sources.is_empty());
    }

    #[test]
    fn a_failed_flush_does_not_commit_upload_or_cleanup_state() {
        #[derive(Default)]
        struct FailingFlush(Vec<u8>);
        impl Write for FailingFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("fixture flush failure"))
            }
        }
        let source = source();
        let mut renderer = KittyRenderer::default();
        let mut writer = FailingFlush::default();
        assert!(
            renderer
                .finish(&mut writer, scene(&source, true, 80, 24).0)
                .is_err()
        );
        assert!(!renderer.sources[&source.key].uploaded);
        assert!(renderer.cleanup(&mut writer).is_err());
        assert_eq!(renderer.sources.len(), 1);
        renderer.cleanup(&mut Vec::new()).unwrap();
        assert!(renderer.sources.is_empty());
    }
}
