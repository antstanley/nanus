//! Exact initial image profiles and checked safety reservations.

use crate::content::{ContentError, IMAGE_BYTES_MAX, ImageDimensions, RECORD_BYTES_MAX};

/// Local validated estimation metadata; variants deliberately do not inherit provider-wide support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProfile {
    /// Anthropic Messages, Opus 5.5, high resolution and 28-pixel patches, version 1.
    AnthropicOpus55HighPatch28V1,
    /// Anthropic Messages, Sonnet 5.5, independently verified, version 1.
    AnthropicSonnet55HighPatch28V1,
    /// `OpenAI` Chat Completions, Astra, high detail and 32-pixel patches, version 1.
    OpenAiAstraHighPatch32V1,
}

impl ImageProfile {
    /// The versioned profile name recorded beside accepted-wire evidence.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AnthropicOpus55HighPatch28V1 => "anthropic-opus55-high-patch28-v1",
            Self::AnthropicSonnet55HighPatch28V1 => "anthropic-sonnet55-high-patch28-v1",
            Self::OpenAiAstraHighPatch32V1 => "openai-astra-high-patch32-v1",
        }
    }

    /// The exact model this profile may describe.
    #[must_use]
    pub const fn model(self) -> &'static str {
        match self {
            Self::AnthropicOpus55HighPatch28V1 => "claude-opus-5-5",
            Self::AnthropicSonnet55HighPatch28V1 => "claude-sonnet-5-5",
            Self::OpenAiAstraHighPatch32V1 => "gpt-6-astra",
        }
    }

    /// Largest image file in bytes.
    #[must_use]
    pub const fn max_image_bytes(self) -> usize {
        IMAGE_BYTES_MAX
    }

    /// Largest request in encoded bytes, independently bounded from image tokens.
    #[must_use]
    pub const fn max_request_bytes(self) -> usize {
        RECORD_BYTES_MAX
    }

    /// Largest image count in an assembled provider request.
    #[must_use]
    pub const fn max_request_images(self) -> usize {
        8
    }

    /// Computes visual patches before provider multiplier or library safety headroom.
    pub fn visual_patches(self, dimensions: ImageDimensions) -> Result<u32, ContentError> {
        let (edge, patch) = match self {
            Self::AnthropicOpus55HighPatch28V1 | Self::AnthropicSonnet55HighPatch28V1 => (2576, 28),
            Self::OpenAiAstraHighPatch32V1 => (1024, 32),
        };
        let ImageDimensions { width, height } = dimensions;
        if width == 0 || height == 0 || width > edge || height > edge {
            return Err(ContentError::new("image edge outside selected profile"));
        }
        let patches = ceil_div(width, patch)?
            .checked_mul(ceil_div(height, patch)?)
            .ok_or_else(|| ContentError::new("image patch arithmetic overflow"))?;
        if self != Self::OpenAiAstraHighPatch32V1 && patches > 4784 {
            return Err(ContentError::new("image exceeds 4784 visual patches"));
        }
        Ok(patches)
    }

    /// Reserves `ceil(5 × (base_charge + 32) / 4)` tokens using checked integers.
    ///
    /// This includes framing and 25% safety headroom; it is not a billed token count.
    pub fn reserved_tokens(self, dimensions: ImageDimensions) -> Result<u32, ContentError> {
        let patches = self.visual_patches(dimensions)?;
        let base = if self == Self::OpenAiAstraHighPatch32V1 {
            ceil_div(
                patches
                    .checked_mul(6)
                    .ok_or_else(|| ContentError::new("image charge overflow"))?,
                5,
            )?
        } else {
            patches
        };
        let charge = base
            .checked_add(32)
            .and_then(|charge| charge.checked_mul(5))
            .ok_or_else(|| ContentError::new("image safety reservation overflow"))?;
        ceil_div(charge, 4)
    }
}

/// Divides upwards without overflowing a pre-division addition.
fn ceil_div(value: u32, divisor: u32) -> Result<u32, ContentError> {
    let whole = value
        .checked_div(divisor)
        .ok_or_else(|| ContentError::new("zero patch divisor"))?;
    let remainder = value
        .checked_rem(divisor)
        .ok_or_else(|| ContentError::new("zero patch divisor"))?;
    whole
        .checked_add(u32::from(remainder != 0))
        .ok_or_else(|| ContentError::new("image rounding overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn each_profile_enforces_its_native_edges_patches_and_upward_reservation() {
        for profile in [
            ImageProfile::AnthropicOpus55HighPatch28V1,
            ImageProfile::AnthropicSonnet55HighPatch28V1,
        ] {
            assert_eq!(
                profile
                    .visual_patches(ImageDimensions {
                        width: 1920,
                        height: 1080
                    })
                    .unwrap(),
                2691
            );
            assert_eq!(
                profile
                    .visual_patches(ImageDimensions {
                        width: 2576,
                        height: 1456
                    })
                    .unwrap(),
                4784
            );
            assert_eq!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 2576,
                        height: 1456
                    })
                    .unwrap(),
                6020
            );
            assert_eq!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 1024,
                        height: 1024
                    })
                    .unwrap(),
                1752
            );
            assert!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 2577,
                        height: 1
                    })
                    .is_err()
            );
            assert!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 2576,
                        height: 1457
                    })
                    .is_err()
            );
        }
        let profile = ImageProfile::OpenAiAstraHighPatch32V1;
        assert_eq!(
            profile
                .reserved_tokens(ImageDimensions {
                    width: 1024,
                    height: 1024
                })
                .unwrap(),
            1577
        );
        assert_eq!(
            profile
                .reserved_tokens(ImageDimensions {
                    width: 1,
                    height: 1
                })
                .unwrap(),
            43
        );
        assert!(
            profile
                .reserved_tokens(ImageDimensions {
                    width: 1025,
                    height: 1
                })
                .is_err()
        );
        assert!(
            profile
                .reserved_tokens(ImageDimensions {
                    width: 1,
                    height: 0
                })
                .is_err()
        );
    }
}
