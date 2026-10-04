//! Where `DeepSeek` image input has evidence, and where it does not.
//!
//! Support is recorded per exact model on the official endpoint, and nowhere else: an alias, a
//! proxy or a changed base URL inherits nothing. The evidence is in `docs/vision-evidence.md`.
//!
//! The context and output ceilings are here too, because an image request is refused without
//! known ceilings. They are what `GET https://api.deepseek.com/models` reported on 2026-10-04:
//! both models take 1,048,576 tokens of combined context and write at most 393,216, and only
//! `deepseek-flash` lists `image` among its input modalities.

use nanus_ports::{ImageInputSupport, ImageProfile, ModelCapabilities};

use crate::{DEFAULT_BASE_URL, DeepSeekConfig, MODEL_FLASH, MODEL_PRO};

/// The ceilings the models endpoint reports, identical for both models.
const fn ceilings(image_input: ImageInputSupport) -> ModelCapabilities {
    ModelCapabilities {
        image_input,
        image_profile: None,
        context_window_tokens: Some(1_048_576),
        max_input_tokens: Some(1_048_576),
        max_output_tokens: Some(393_216),
    }
}

/// Returns the image capabilities of `model` on `config`'s endpoint.
///
/// Flash read PNG, JPEG, WebP and a still GIF live, and named the call that produced the picture;
/// Pro was shown live not to read one, and the models endpoint agrees. Both are recorded on the
/// official endpoint only; any other model or endpoint reports nothing.
pub fn capabilities(config: &DeepSeekConfig, model: &str) -> ModelCapabilities {
    if config.base_url().trim_end_matches('/') != DEFAULT_BASE_URL {
        return ModelCapabilities::default();
    }
    match model {
        MODEL_FLASH => ModelCapabilities {
            image_profile: Some(ImageProfile::DeepSeekFlashAreaV1),
            ..ceilings(ImageInputSupport::Supported)
        },
        MODEL_PRO => ceilings(ImageInputSupport::Unsupported),
        _ => ModelCapabilities::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_flash_on_the_official_endpoint_reads_images() {
        let official = DeepSeekConfig::new(MODEL_FLASH, "fixture-key");
        let flash = capabilities(&official, MODEL_FLASH);
        assert_eq!(flash.image_input, ImageInputSupport::Supported);
        assert_eq!(flash.image_profile, Some(ImageProfile::DeepSeekFlashAreaV1));
        assert!(flash.require_image_profile(MODEL_FLASH).is_ok());
        // An image request needs known ceilings, and these are the ones the endpoint reported.
        assert_eq!(flash.context_window_tokens, Some(1_048_576));
        assert_eq!(flash.max_output_tokens, Some(393_216));

        let pro = capabilities(&official, MODEL_PRO);
        assert_eq!(pro.image_input, ImageInputSupport::Unsupported);
        assert!(pro.require_image_profile(MODEL_PRO).is_err());

        for model in [
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek-v4-flash",
            "",
        ] {
            let caps = capabilities(&official, model);
            assert_eq!(caps.image_input, ImageInputSupport::Unknown, "{model}");
            assert_eq!(caps.image_profile, None, "{model}");
        }
        let proxy =
            DeepSeekConfig::with_base_url(MODEL_FLASH, "fixture-key", "https://proxy.example");
        assert_eq!(
            capabilities(&proxy, MODEL_FLASH),
            ModelCapabilities::default(),
            "a changed endpoint inherits nothing"
        );
        // A trailing slash is still the official endpoint.
        let slash =
            DeepSeekConfig::with_base_url(MODEL_FLASH, "fixture-key", "https://api.deepseek.com/");
        assert_eq!(
            capabilities(&slash, MODEL_FLASH).image_input,
            ImageInputSupport::Supported
        );
    }
}
