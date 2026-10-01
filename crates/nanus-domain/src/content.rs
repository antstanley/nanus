//! Model-neutral content bounds and verified inline media.

use std::io::{Cursor, Write};

use base64::Engine as _;
use image::{ImageDecoder as _, ImageFormat, ImageReader};

use crate::ContentBlock;

/// Largest decoded file accepted as an inline tool image (512 KiB).
pub const IMAGE_BYTES_MAX: usize = 512 * 1024;
/// Largest padded base64 encoding of an accepted image.
pub const IMAGE_BASE64_MAX: usize = 699_052;
/// Maximum blocks in one result.
pub const CONTENT_BLOCKS_MAX: usize = 32;
/// Maximum images in one result.
pub const RESULT_IMAGES_MAX: usize = 4;
/// Maximum serialized record or provider request size (4 MiB).
pub const RECORD_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Default maximum serialized session size (64 MiB).
pub const SESSION_BYTES_MAX: usize = 64 * 1024 * 1024;

/// Why inline content cannot safely be retained or transmitted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid tool content: {reason}")]
pub struct ContentError {
    /// The violated bound or malformed media.
    pub reason: String,
}

impl ContentError {
    /// Names an invalid content condition.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Image dimensions verified against a complete, bounded PNG/JPEG decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDimensions {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Verifies encoded size, media magic, dimensions and complete decode in that order.
///
/// The model-neutral bound admits every initial profile. A request must also enforce
/// its selected profile's tighter edge/patch limits before reaching the provider.
/// No file or URL is opened. Allocation is capped at 32 MiB, with dimensions inspected
/// before allocating a pixel buffer; only PNG and JPEG decoders are compiled in.
pub fn validate_image(media_type: &str, data: &str) -> Result<ImageDimensions, ContentError> {
    if data.is_empty() || data.len() > IMAGE_BASE64_MAX {
        return Err(ContentError::new("empty or oversized base64 image"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ContentError::new("malformed base64 image"))?;
    if bytes.is_empty() || bytes.len() > IMAGE_BYTES_MAX {
        return Err(ContentError::new("empty or oversized image file"));
    }
    let format =
        image::guess_format(&bytes).map_err(|_| ContentError::new("invalid image magic"))?;
    if !matches!(
        (media_type, format),
        ("image/png", ImageFormat::Png)
            | ("image/jpeg", ImageFormat::Jpeg)
            | ("image/webp", ImageFormat::WebP)
            | ("image/gif", ImageFormat::Gif)
    ) {
        return Err(ContentError::new(
            "media type conflicts with PNG/JPEG/WebP/GIF magic",
        ));
    }
    if format == ImageFormat::Gif && is_animated_gif(&bytes) {
        // Providers read a still: an animation would be silently reduced to one frame.
        return Err(ContentError::new("animated GIF images are not supported"));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2576);
    limits.max_image_height = Some(2576);
    limits.max_alloc = Some(32 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| ContentError::new(error.to_string()))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 2576 || height > 2576 {
        return Err(ContentError::new("image dimensions outside library bounds"));
    }
    let pixels = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| ContentError::new(error.to_string()))?;
    if pixels.width() != width || pixels.height() != height {
        return Err(ContentError::new(
            "decoded image dimensions disagree with header",
        ));
    }
    Ok(ImageDimensions { width, height })
}

/// Whether a GIF holds more than one frame, decoding no more than the second.
fn is_animated_gif(bytes: &[u8]) -> bool {
    use image::AnimationDecoder as _;
    image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .is_ok_and(|decoder| decoder.into_frames().take(2).count() > 1)
}

/// Verifies a present block list; absence is the separate legacy-text meaning.
pub fn validate_blocks(blocks: &[ContentBlock]) -> Result<(), ContentError> {
    if blocks.is_empty() || blocks.len() > CONTENT_BLOCKS_MAX {
        return Err(ContentError::new("a result requires 1–32 content blocks"));
    }
    let mut images = 0_usize;
    for block in blocks {
        if let ContentBlock::Image {
            media_type,
            data_base64,
        } = block
        {
            images = images.saturating_add(1);
            if images > RESULT_IMAGES_MAX {
                return Err(ContentError::new("more than four images in a tool result"));
            }
            validate_image(media_type, data_base64)?;
        }
    }
    serialized_size(blocks, RECORD_BYTES_MAX)?;
    Ok(())
}

/// Reads optional blocks through the same validation as a fresh tool result.
pub fn deserialize_blocks<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<ContentBlock>>, D::Error> {
    use serde::Deserialize as _;
    let blocks = Option::<Vec<ContentBlock>>::deserialize(deserializer)?;
    if let Some(blocks) = &blocks {
        validate_blocks(blocks).map_err(serde::de::Error::custom)?;
    }
    Ok(blocks)
}

/// Counts actual JSON bytes without allocating a serialized copy, refusing overflow.
pub fn serialized_size<T: serde::Serialize + ?Sized>(
    value: &T,
    max: usize,
) -> Result<usize, ContentError> {
    let mut counter = BoundedCounter { bytes: 0, max };
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| ContentError::new(error.to_string()))?;
    Ok(counter.bytes)
}

struct BoundedCounter {
    bytes: usize,
    max: usize,
}
impl Write for BoundedCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(buffer.len())
            .filter(|next| *next <= self.max)
            .ok_or_else(|| std::io::Error::other("serialized content exceeds byte limit"))?;
        self.bytes = next;
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(format: ImageFormat, width: u32, height: u32) -> String {
        let pixels = image::DynamicImage::new_rgb8(width, height);
        let mut out = Cursor::new(Vec::new());
        pixels.write_to(&mut out, format).unwrap();
        base64::engine::general_purpose::STANDARD.encode(out.into_inner())
    }

    #[test]
    fn png_jpeg_webp_and_gif_require_complete_matching_media_and_bounded_dimensions() {
        for (format, media) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::WebP, "image/webp"),
            (ImageFormat::Gif, "image/gif"),
        ] {
            let data = encoded(format, 7, 9);
            assert_eq!(
                validate_image(media, &data).unwrap(),
                ImageDimensions {
                    width: 7,
                    height: 9
                }
            );
            assert!(validate_image("image/bmp", &data).is_err());
            let wrong = if media == "image/png" {
                "image/jpeg"
            } else {
                "image/png"
            };
            assert!(validate_image(wrong, &data).is_err());
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .unwrap();
            assert!(
                validate_image(
                    media,
                    &base64::engine::general_purpose::STANDARD.encode(&bytes[..16])
                )
                .is_err()
            );
            assert!(validate_image(media, &encoded(format, 2577, 1)).is_err());
        }
    }

    /// A still GIF is accepted, and an animation is refused rather than silently reduced to its
    /// first frame.
    #[test]
    fn an_animated_gif_is_refused_and_a_still_one_is_not() {
        let frame = || image::Frame::new(image::RgbaImage::new(4, 4));
        let encode = |frames: Vec<image::Frame>| {
            let mut out = Vec::new();
            image::codecs::gif::GifEncoder::new(&mut out)
                .encode_frames(frames)
                .unwrap();
            base64::engine::general_purpose::STANDARD.encode(out)
        };
        assert!(validate_image("image/gif", &encode(vec![frame()])).is_ok());
        let error = validate_image("image/gif", &encode(vec![frame(), frame()])).unwrap_err();
        assert!(error.to_string().contains("animated"), "{error}");
    }

    #[test]
    fn malformed_empty_and_oversized_image_data_is_refused() {
        for bad in ["", "%%%", "AAAA", "a==="] {
            assert!(validate_image("image/png", bad).is_err());
        }
        assert!(
            validate_image("image/png", &"A".repeat(IMAGE_BASE64_MAX.saturating_add(1))).is_err()
        );
        let oversized = vec![0_u8; IMAGE_BYTES_MAX.saturating_add(1)];
        assert!(
            validate_image(
                "image/png",
                &base64::engine::general_purpose::STANDARD.encode(oversized)
            )
            .is_err()
        );
    }

    #[test]
    fn a_result_requires_nonempty_bounded_blocks_and_at_most_four_images() {
        let image = ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: encoded(ImageFormat::Png, 1, 1),
        };
        assert!(validate_blocks(&[]).is_err());
        assert!(validate_blocks(&vec![ContentBlock::Text("text".into()); 32]).is_ok());
        assert!(validate_blocks(&vec![ContentBlock::Text("text".into()); 33]).is_err());
        assert!(validate_blocks(&vec![image.clone(); 4]).is_ok());
        assert!(validate_blocks(&vec![image; 5]).is_err());
        assert!(validate_blocks(&[ContentBlock::Text("x".repeat(RECORD_BYTES_MAX))]).is_err());
    }

    #[test]
    fn the_byte_counter_counts_escaped_json_and_refuses_a_one_byte_overflow() {
        assert_eq!(serialized_size("\n", 4).unwrap(), 4);
        assert!(serialized_size("\n", 3).is_err());
        assert_eq!(serialized_size("é", 4).unwrap(), 4);
    }
}
