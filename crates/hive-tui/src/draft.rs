//! Validated, terminal-independent attachments and immutable prompt intent.

use crate::images::{MAX_DIMENSION, MAX_PIXELS};
use agent_client_protocol::schema::v2;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    io::{self, Write},
    sync::Arc,
};

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    #[error(
        "Image attachments exceed 8 MiB or four images; remove an attachment or use a smaller image"
    )]
    Limit,
    #[error("Image MIME type does not match its bytes; copy a valid PNG, JPEG, or WebP image")]
    Format,
    #[error(transparent)]
    Image(#[from] crate::images::PrepareError),
    #[error("Prompt exceeds the 15 MiB serialized request limit; shorten text or remove an image")]
    PromptSize,
}

#[derive(Debug)]
pub struct Attachment {
    pub bytes: Arc<[u8]>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    block: v2::ContentBlock,
}

impl Attachment {
    pub fn new(mime: String, bytes: Vec<u8>) -> Result<Arc<Self>, DraftError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(DraftError::Limit);
        }
        let format = match mime.as_str() {
            "image/png" => image::ImageFormat::Png,
            "image/jpeg" => image::ImageFormat::Jpeg,
            "image/webp" => image::ImageFormat::WebP,
            _ => return Err(DraftError::Format),
        };
        if image::guess_format(&bytes).ok() != Some(format) {
            return Err(DraftError::Format);
        }
        let mut reader = image::ImageReader::with_format(io::Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        limits.max_alloc = Some(MAX_PIXELS * 4);
        reader.limits(limits);
        let decoder = reader
            .into_decoder()
            .map_err(crate::images::PrepareError::from)?;
        let (width, height) = image::ImageDecoder::dimensions(&decoder);
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(
                crate::images::PrepareError::from(crate::images::ImageError::Dimensions {
                    width,
                    height,
                })
                .into(),
            );
        }
        // Header inspection cannot catch a truncated or corrupt compressed pixel stream.
        image::DynamicImage::from_decoder(decoder).map_err(crate::images::PrepareError::from)?;
        let encoded = STANDARD.encode(&bytes);
        let block = v2::ContentBlock::Image(v2::ImageContent::new(encoded, mime.clone()));
        Ok(Arc::new(Self {
            bytes: bytes.into(),
            mime,
            width,
            height,
            block,
        }))
    }

    pub fn from_native(image: arboard::ImageData<'_>) -> Result<Arc<Self>, DraftError> {
        if image.width == 0
            || image.height == 0
            || image.width > MAX_DIMENSION as usize
            || image.height > MAX_DIMENSION as usize
            || image.width.saturating_mul(image.height) > MAX_PIXELS as usize
        {
            return Err(DraftError::Limit);
        }
        let mut output = LimitedBytes(Vec::new());
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut output),
            &image.bytes,
            image.width as u32,
            image.height as u32,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(crate::images::PrepareError::from)?;
        Self::new("image/png".into(), output.0)
    }

    pub fn native(&self) -> Result<arboard::ImageData<'static>, DraftError> {
        // Admission already validated these immutable bytes under decoder limits.
        let pixels = image::load_from_memory(&self.bytes)
            .map_err(crate::images::PrepareError::from)?
            .into_rgba8();
        Ok(arboard::ImageData {
            width: self.width as usize,
            height: self.height as usize,
            bytes: pixels.into_raw().into(),
        })
    }
}

struct LimitedBytes(Vec<u8>);
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("clipboard PNG exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct PromptDraft {
    pub text: String,
    pub images: Vec<Arc<Attachment>>,
}

impl From<&str> for PromptDraft {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}
impl From<String> for PromptDraft {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl PromptDraft {
    pub fn blocks(&self) -> Vec<v2::ContentBlock> {
        let mut blocks = Vec::with_capacity(1 + self.images.len());
        if !self.text.is_empty() {
            blocks.push(self.text.clone().into());
        }
        blocks.extend(self.images.iter().map(|image| image.block.clone()));
        blocks
    }
}

pub fn check_request(request: &v2::PromptRequest) -> Result<(), DraftError> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > 15 * 1024 * 1024 {
                return Err(io::Error::other("prompt too large"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    // Count the complete JSON-RPC envelope, including escaping and metadata, without a second buffer.
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        jsonrpc: &'static str,
        id: u64,
        method: &'static str,
        params: &'a v2::PromptRequest,
    }
    serde_json::to_writer(
        Counter(0),
        &Envelope {
            jsonrpc: "2.0",
            id: u64::MAX,
            method: "session/prompt",
            params: request,
        },
    )
    .map_err(|_| DraftError::PromptSize)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut bytes),
            &[1, 2, 3, 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        bytes
    }
    #[test]
    fn image_only_drafts_produce_acp_image_blocks_and_reject_corrupt_pixels() {
        let image = Attachment::new("image/png".into(), png()).unwrap();
        let draft = PromptDraft {
            text: String::new(),
            images: vec![image],
        };
        assert!(matches!(&draft.blocks()[..], [v2::ContentBlock::Image(_)]));
        assert!(Attachment::new("image/jpeg".into(), png()).is_err());
        assert!(Attachment::new("image/png".into(), png()[..33].to_vec()).is_err());
    }

    #[test]
    fn image_byte_dimension_and_serialized_prompt_limits_are_enforced_before_submission() {
        assert!(matches!(
            Attachment::new("image/png".into(), vec![0; MAX_IMAGE_BYTES + 1]),
            Err(DraftError::Limit)
        ));
        assert!(
            Attachment::from_native(arboard::ImageData {
                width: MAX_DIMENSION as usize + 1,
                height: 1,
                bytes: Vec::new().into()
            })
            .is_err()
        );
        let small = v2::PromptRequest::new("session", vec!["describe image".into()]);
        assert!(check_request(&small).is_ok());
        // UTF-8 length alone misses the doubling of quotes during JSON serialization.
        let escaped = v2::PromptRequest::new(
            "session",
            vec!["\"".repeat(crate::editor::MAX_IMPORT_BYTES).into()],
        );
        assert!(matches!(
            check_request(&escaped),
            Err(DraftError::PromptSize)
        ));
    }
}
