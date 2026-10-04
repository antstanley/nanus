//! Exact initial image profiles and checked safety reservations.

use crate::content::{ContentError, IMAGE_BYTES_MAX, ImageDimensions, RECORD_BYTES_MAX};

/// Local validated estimation metadata; variants deliberately do not inherit provider-wide support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProfile {
    /// Anthropic Messages, Opus 5.5, high resolution and 28-pixel patches, version 1.
    AnthropicOpus55HighPatch28V1,
    /// Anthropic Messages, Sonnet 5.5, independently verified, version 1.
    AnthropicSonnet55HighPatch28V1,
    /// `OpenAI` Astra, high detail and 32-pixel patches, version 1.
    OpenAiAstraHighPatch32V1,
    /// `OpenAI` GPT-6.1 Sol, high detail and 32-pixel patches, version 1.
    OpenAiSol61HighPatch32V1,
    /// `OpenAI` GPT-6 Luna, high detail and 32-pixel patches, version 1.
    OpenAiLuna6HighPatch32V1,
    /// `OpenAI` GPT-5.6 Sol, high detail and 32-pixel patches, version 1.
    OpenAiSol56HighPatch32V1,
    /// `OpenAI` GPT-5.6 Terra, high detail and 32-pixel patches, version 1.
    OpenAiTerra56HighPatch32V1,
    /// `OpenAI` GPT-5.6 Luna, high detail and 32-pixel patches, version 1.
    OpenAiLuna56HighPatch32V1,
    /// `DeepSeek` Flash on chat completions, priced by a measured area rule, version 1.
    ///
    /// `DeepSeek` documents an image's limits but not its price, so the reservation rests on a
    /// measurement (see `docs/vision-evidence.md`): a fixed charge plus a charge per 1,024 pixels,
    /// at about twice the measured slope.
    DeepSeekFlashAreaV1,
}

impl ImageProfile {
    /// The versioned profile name recorded beside accepted-wire evidence.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AnthropicOpus55HighPatch28V1 => "anthropic-opus55-high-patch28-v1",
            Self::AnthropicSonnet55HighPatch28V1 => "anthropic-sonnet55-high-patch28-v1",
            Self::OpenAiAstraHighPatch32V1 => "openai-astra-high-patch32-v1",
            Self::OpenAiSol61HighPatch32V1 => "openai-sol61-high-patch32-v1",
            Self::OpenAiLuna6HighPatch32V1 => "openai-luna6-high-patch32-v1",
            Self::OpenAiSol56HighPatch32V1 => "openai-sol56-high-patch32-v1",
            Self::OpenAiTerra56HighPatch32V1 => "openai-terra56-high-patch32-v1",
            Self::OpenAiLuna56HighPatch32V1 => "openai-luna56-high-patch32-v1",
            Self::DeepSeekFlashAreaV1 => "deepseek-flash-area-v1",
        }
    }

    /// The exact model this profile may describe.
    #[must_use]
    pub const fn model(self) -> &'static str {
        match self {
            Self::AnthropicOpus55HighPatch28V1 => "claude-opus-5-5",
            Self::AnthropicSonnet55HighPatch28V1 => "claude-sonnet-5-5",
            Self::OpenAiAstraHighPatch32V1 => "gpt-6-astra",
            Self::OpenAiSol61HighPatch32V1 => "gpt-6.1-sol",
            Self::OpenAiLuna6HighPatch32V1 => "gpt-6-luna",
            Self::OpenAiSol56HighPatch32V1 => "gpt-5.6-sol",
            Self::OpenAiTerra56HighPatch32V1 => "gpt-5.6-terra",
            Self::OpenAiLuna56HighPatch32V1 => "gpt-5.6-luna",
            Self::DeepSeekFlashAreaV1 => "deepseek-flash",
        }
    }

    /// The profile for an exact `OpenAI` model id, when one exists.
    ///
    /// A profile is not inherited by a neighbouring model: an id not named here has none, whatever
    /// its family.
    #[must_use]
    pub fn for_openai_model(model: &str) -> Option<Self> {
        [
            Self::OpenAiAstraHighPatch32V1,
            Self::OpenAiSol61HighPatch32V1,
            Self::OpenAiLuna6HighPatch32V1,
            Self::OpenAiSol56HighPatch32V1,
            Self::OpenAiTerra56HighPatch32V1,
            Self::OpenAiLuna56HighPatch32V1,
        ]
        .into_iter()
        .find(|profile| profile.model() == model)
    }

    /// Whether this is one of the `OpenAI` profiles, which share one patch rule: 32-pixel patches
    /// on a 1024-pixel edge, charged at 1.2 tokens a patch.
    #[must_use]
    pub const fn is_openai(self) -> bool {
        !matches!(
            self,
            Self::AnthropicOpus55HighPatch28V1
                | Self::AnthropicSonnet55HighPatch28V1
                | Self::DeepSeekFlashAreaV1
        )
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
            // `DeepSeek` takes 8192 pixels a side; the library's own bound is the tighter one.
            Self::DeepSeekFlashAreaV1 => (2576, 0),
            _ => (1024, 32),
        };
        let ImageDimensions { width, height } = dimensions;
        if width == 0 || height == 0 || width > edge || height > edge {
            return Err(ContentError::new("image edge outside selected profile"));
        }
        if self == Self::DeepSeekFlashAreaV1 {
            // Not patches: units of 1,024 pixels, which is how the measured price scales.
            return ceil_div(
                width
                    .checked_mul(height)
                    .ok_or_else(|| ContentError::new("image area overflow"))?,
                1024,
            );
        }
        let patches = ceil_div(width, patch)?
            .checked_mul(ceil_div(height, patch)?)
            .ok_or_else(|| ContentError::new("image patch arithmetic overflow"))?;
        if !self.is_openai() && self != Self::DeepSeekFlashAreaV1 && patches > 4784 {
            return Err(ContentError::new("image exceeds 4784 visual patches"));
        }
        Ok(patches)
    }

    /// Reserves `ceil(5 × (base_charge + 32) / 4)` tokens using checked integers.
    ///
    /// This includes framing and 25% safety headroom; it is not a billed token count.
    pub fn reserved_tokens(self, dimensions: ImageDimensions) -> Result<u32, ContentError> {
        let patches = self.visual_patches(dimensions)?;
        if self == Self::DeepSeekFlashAreaV1 {
            // A fixed 256 plus one per 1,024 pixels: about twice what was measured, and already
            // a safety figure, so the 25% headroom the patch profiles add is not added again.
            return patches
                .checked_add(256)
                .ok_or_else(|| ContentError::new("image safety reservation overflow"));
        }
        let base = if self.is_openai() {
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
    }

    #[test]
    fn every_openai_profile_shares_one_patch_rule_and_belongs_to_one_exact_model() {
        for profile in [
            ImageProfile::OpenAiAstraHighPatch32V1,
            ImageProfile::OpenAiSol61HighPatch32V1,
            ImageProfile::OpenAiLuna6HighPatch32V1,
            ImageProfile::OpenAiSol56HighPatch32V1,
            ImageProfile::OpenAiTerra56HighPatch32V1,
            ImageProfile::OpenAiLuna56HighPatch32V1,
        ] {
            assert!(profile.is_openai(), "{}", profile.id());
            assert_eq!(
                ImageProfile::for_openai_model(profile.model()),
                Some(profile)
            );
            assert_eq!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 1024,
                        height: 1024
                    })
                    .unwrap(),
                1577
            );
            assert!(
                profile
                    .reserved_tokens(ImageDimensions {
                        width: 1025,
                        height: 1
                    })
                    .is_err()
            );
        }
        assert_eq!(ImageProfile::for_openai_model("gpt-5.5"), None);
        assert_eq!(ImageProfile::for_openai_model("claude-opus-5-5"), None);
        assert!(!ImageProfile::AnthropicOpus55HighPatch28V1.is_openai());
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

    /// The reservation must stay above what `deepseek-flash` was measured to charge, live,
    /// 2026-10-04: 209 prompt tokens for a 28x28 or 256x256 picture, 217 for 640x360, 391 for
    /// 1024x576, 677 for 1024x1024 and 1017 for 2000x1000 (each including a fixed ~200 for the
    /// attachment framing). A reservation under a measured cost would let a request through that
    /// the model then refuses on context.
    #[test]
    fn the_deepseek_reservation_covers_every_measured_cost_and_refuses_oversize() {
        let profile = ImageProfile::DeepSeekFlashAreaV1;
        assert_eq!(profile.model(), "deepseek-flash");
        assert!(!profile.is_openai());
        for ((width, height), measured) in [
            ((28, 28), 209),
            ((256, 256), 209),
            ((640, 360), 217),
            ((1024, 576), 391),
            ((1024, 1024), 677),
            ((2000, 1000), 1017),
        ] {
            let reserved = profile
                .reserved_tokens(ImageDimensions { width, height })
                .unwrap();
            assert!(
                reserved >= measured,
                "{width}x{height} reserves {reserved}, below the measured {measured}"
            );
        }
        assert_eq!(
            profile
                .reserved_tokens(ImageDimensions {
                    width: 1024,
                    height: 1024
                })
                .unwrap(),
            1280
        );
        for (width, height) in [(0, 10), (10, 0), (2577, 10), (10, 2577)] {
            assert!(
                profile
                    .reserved_tokens(ImageDimensions { width, height })
                    .is_err(),
                "{width}x{height} must be outside the profile"
            );
        }
    }
}
