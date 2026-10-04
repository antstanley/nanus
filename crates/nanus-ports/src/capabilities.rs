//! Per-model capabilities, queried locally without credentials or network access.

use nanus_domain::{ContentBlock, Message};

use crate::{ChatRequest, LlmError, LlmResult};
pub use nanus_domain::image_profile::ImageProfile;

/// Whether a model/protocol pair has verified image input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageInputSupport {
    /// Recorded accepted-wire and live-follow-up evidence permits images.
    Supported,
    /// This protocol or model explicitly does not support images here.
    Unsupported,
    /// Image acceptance has not passed its gate; images are refused.
    #[default]
    Unknown,
}

/// Optional metadata for the exact model on this adapter's configured protocol/vendor.
///
/// Missing ceilings are unknown, not inferred. Stock text-only callers retain their
/// existing behavior; embedding hosts may require all ceilings explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Image acceptance for this exact combination.
    pub image_input: ImageInputSupport,
    /// Validated image profile; `Supported` requires a model-matching profile.
    pub image_profile: Option<ImageProfile>,
    /// Combined input/output context ceiling.
    pub context_window_tokens: Option<u32>,
    /// Model input ceiling, when known.
    pub max_input_tokens: Option<u32>,
    /// Model output ceiling, when known.
    pub max_output_tokens: Option<u32>,
}

impl ModelCapabilities {
    /// Resolves a supported profile without a guessed fallback.
    pub fn require_image_profile(self, model: &str) -> LlmResult<ImageProfile> {
        if self.image_input != ImageInputSupport::Supported {
            return Err(LlmError::Unsupported {
                feature: format!("image input is {:?} for {model}", self.image_input),
            });
        }
        self.image_profile
            .filter(|profile| profile.model() == model)
            .ok_or_else(|| LlmError::Unsupported {
                feature: "missing or mismatched image profile".into(),
            })
    }
}

/// Whether any retained tool message contains pixels.
#[must_use]
pub fn has_images(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        matches!(message, Message::Tool { content_blocks: Some(blocks), .. }
        if blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. })))
    })
}

/// Validates all retained images on every request, including after a model switch.
///
/// Adapters call this before encoding or starting HTTP, even when used without a runner.
pub fn validate_image_input(
    capabilities: ModelCapabilities,
    request: &ChatRequest,
) -> LlmResult<()> {
    validate_images(capabilities, &request.model, &request.messages, true)
}

/// Validates every original image before fitting, without applying a fitted-request count cap.
/// # Errors
/// Refuses unsupported/mismatched profiles, malformed pixels and invalid per-image dimensions.
pub fn validate_history_image_input(
    capabilities: ModelCapabilities,
    model: &str,
    messages: &[Message],
) -> LlmResult<()> {
    validate_images(capabilities, model, messages, false)
}

fn validate_images(
    capabilities: ModelCapabilities,
    model: &str,
    messages: &[Message],
    request_count: bool,
) -> LlmResult<()> {
    if !has_images(messages) {
        return Ok(());
    }
    let profile = capabilities.require_image_profile(model)?;
    let mut images = 0_usize;
    for message in messages {
        if let Message::Tool {
            content_blocks: Some(blocks),
            ..
        } = message
        {
            nanus_domain::content::validate_blocks(blocks).map_err(|error| {
                LlmError::Unsupported {
                    feature: error.to_string(),
                }
            })?;
            for block in blocks {
                if let ContentBlock::Image {
                    media_type,
                    data_base64,
                } = block
                {
                    images = images.saturating_add(1);
                    if request_count && images > profile.max_request_images() {
                        return Err(LlmError::Unsupported {
                            feature: "more than eight request images".into(),
                        });
                    }
                    let dimensions = nanus_domain::content::validate_image(media_type, data_base64)
                        .map_err(|error| LlmError::Unsupported {
                            feature: error.to_string(),
                        })?;
                    profile
                        .reserved_tokens(dimensions)
                        .map_err(|error| LlmError::Unsupported {
                            feature: error.to_string(),
                        })?;
                }
            }
        }
    }
    Ok(())
}

/// The single app-generated label tying a Chat Completions attachment to its call.
#[must_use]
pub fn attachment_label(call_id: &nanus_domain::ToolCallId, is_error: bool) -> String {
    format!(
        "[tool image attachment: {}; is_error={is_error}]",
        call_id.as_str()
    )
}

/// Conservative cost of the actual assembled wire body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestEstimate {
    /// Input charge including schemas, framing, labels and validated visual tokens.
    pub input_tokens: u32,
    /// Serialized body bytes, including inline media.
    pub request_bytes: usize,
    /// Retained images in the request.
    pub images: usize,
    /// Combined output and separately bounded reasoning reservation.
    pub reservation: u32,
}

impl RequestEstimate {
    /// Checks model/caller ceilings and request bounds before any HTTP.
    pub fn fits(self, caps: ModelCapabilities, request: &ChatRequest) -> bool {
        let budget = request
            .context_budget
            .unwrap_or(u32::MAX)
            .min(caps.context_window_tokens.unwrap_or(u32::MAX));
        self.request_bytes <= nanus_domain::content::RECORD_BYTES_MAX
            && self.images <= 8
            && self.input_tokens <= caps.max_input_tokens.unwrap_or(u32::MAX)
            && self
                .input_tokens
                .checked_add(self.reservation)
                .is_some_and(|total| total <= budget)
    }
}

fn invalid(message: impl Into<String>) -> LlmError {
    LlmError::Unsupported {
        feature: message.into(),
    }
}

/// Counts the provider's assembled body; encoded pixels are replaced by dimension charges.
///
/// Text and JSON are conservatively charged at one serialized byte per token. Image payload
/// bytes count toward the request byte limit, never toward visual tokens. Arithmetic rounds
/// upward and refuses overflow. Callers can elide whole original turns when `fits` is false.
pub fn estimate_payload(
    caps: ModelCapabilities,
    request: &ChatRequest,
    payload: &serde_json::Value,
) -> LlmResult<RequestEstimate> {
    let image_request = has_images(&request.messages);
    let profile = if image_request {
        Some(caps.require_image_profile(&request.model)?)
    } else {
        None
    };
    if image_request
        && (caps.context_window_tokens.is_none()
            || caps.max_input_tokens.is_none()
            || caps.max_output_tokens.is_none()
            || request.max_tokens.is_none())
    {
        return Err(invalid(
            "image requests require explicit output and known model ceilings",
        ));
    }
    let output = request.max_tokens.unwrap_or_else(|| {
        ["max_tokens", "max_completion_tokens", "max_output_tokens"]
            .iter()
            .find_map(|key| payload[*key].as_u64().and_then(|n| u32::try_from(n).ok()))
            .unwrap_or_default()
    });
    if output == 0 && image_request || caps.max_output_tokens.is_some_and(|max| output > max) {
        return Err(invalid(
            "output reservation exceeds model ceiling or is zero",
        ));
    }
    let reservation = output
        .checked_add(request.separate_reasoning_tokens)
        .ok_or_else(|| invalid("output/reasoning reservation overflow"))?;
    let (images, visual) = visual_cost(&request.messages, profile)?;
    let request_bytes = nanus_domain::content::serialized_size(payload, usize::MAX)
        .map_err(|error| invalid(error.to_string()))?;
    let encoded_pixels = encoded_image_bytes(payload)?;
    let text = request_bytes
        .checked_sub(encoded_pixels)
        .ok_or_else(|| invalid("image byte estimate exceeds body"))?;
    let input_tokens = u32::try_from(text)
        .ok()
        .and_then(|n| n.checked_add(visual))
        .ok_or_else(|| invalid("input estimate overflow"))?;
    Ok(RequestEstimate {
        input_tokens,
        request_bytes,
        images,
        reservation,
    })
}

fn visual_cost(messages: &[Message], profile: Option<ImageProfile>) -> LlmResult<(usize, u32)> {
    let mut images = 0_usize;
    let mut visual = 0_u32;
    for message in messages {
        if let Message::Tool {
            content_blocks: Some(blocks),
            ..
        } = message
        {
            nanus_domain::content::validate_blocks(blocks).map_err(|e| invalid(e.to_string()))?;
            for block in blocks {
                if let ContentBlock::Image {
                    media_type,
                    data_base64,
                } = block
                {
                    let profile = profile.ok_or_else(|| invalid("image profile missing"))?;
                    let dimensions = nanus_domain::content::validate_image(media_type, data_base64)
                        .map_err(|e| invalid(e.to_string()))?;
                    let tokens = profile
                        .reserved_tokens(dimensions)
                        .map_err(|e| invalid(e.to_string()))?;
                    visual = visual
                        .checked_add(tokens)
                        .ok_or_else(|| invalid("visual estimate overflow"))?;
                    images = images
                        .checked_add(1)
                        .ok_or_else(|| invalid("image count overflow"))?;
                }
            }
        }
        if let Message::Assistant {
            replay: Some(replay),
            ..
        } = message
        {
            replay.validate().map_err(|e| invalid(e.to_string()))?;
        }
    }
    Ok((images, visual))
}

// Inspect only message content arrays, never arbitrary tool arguments or schemas. A
// JSON argument that happens to say `type=image` still costs its full serialized bytes.
fn encoded_image_bytes(payload: &serde_json::Value) -> LlmResult<usize> {
    let mut total = 0_usize;
    if let Some(messages) = payload["messages"].as_array() {
        for message in messages {
            for key in ["content", "content_blocks"] {
                if let Some(blocks) = message[key].as_array() {
                    total = total
                        .checked_add(block_image_bytes(blocks)?)
                        .ok_or_else(|| invalid("encoded image estimate overflow"))?;
                }
            }
        }
    }
    Ok(total)
}

fn block_image_bytes(blocks: &[serde_json::Value]) -> LlmResult<usize> {
    let mut total = 0_usize;
    for block in blocks {
        let payload = match block["type"].as_str() {
            Some("image") => block
                .get("data_base64")
                .or_else(|| block["source"].get("data")),
            Some("image_url") => block["image_url"].get("url"),
            Some("tool_result") => {
                if let Some(nested) = block["content"].as_array() {
                    total = total
                        .checked_add(block_image_bytes(nested)?)
                        .ok_or_else(|| invalid("encoded image estimate overflow"))?;
                }
                None
            }
            _ => None,
        };
        if let Some(payload) = payload {
            let bytes = nanus_domain::content::serialized_size(payload, usize::MAX)
                .map_err(|e| invalid(e.to_string()))?
                .saturating_sub(2);
            total = total
                .checked_add(bytes)
                .ok_or_else(|| invalid("encoded image estimate overflow"))?;
        }
    }
    Ok(total)
}

/// Validates a direct adapter request against the same assembled cost used by the runner.
pub fn validate_estimate(
    caps: ModelCapabilities,
    request: &ChatRequest,
    estimate: RequestEstimate,
) -> LlmResult<()> {
    if !estimate.fits(caps, request) {
        return Err(invalid(
            "context-fit failure: input, reservation, image count or request bytes exceed limits",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use nanus_domain::{ContentBlock, ToolCallId};
    use serde_json::json;

    fn caps(profile: ImageProfile) -> ModelCapabilities {
        ModelCapabilities {
            image_input: ImageInputSupport::Supported,
            image_profile: Some(profile),
            context_window_tokens: Some(1_000_000),
            max_input_tokens: Some(1_000_000),
            max_output_tokens: Some(128_000),
        }
    }

    fn pixels(width: u32, height: u32) -> ContentBlock {
        let image = image::DynamicImage::new_rgb8(width, height);
        let mut output = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(output.into_inner()),
        }
    }

    fn request(profile: ImageProfile, block: &ContentBlock, count: usize) -> ChatRequest {
        let mut messages = vec![Message::user("inspect")];
        for index in 0..count {
            messages.push(Message::Tool {
                call_id: ToolCallId::new(format!("call-{index}")),
                content: "summary".into(),
                content_blocks: Some(vec![block.clone()]),
                is_error: false,
            });
        }
        ChatRequest::new(profile.model(), messages).with_max_tokens(8192)
    }

    #[test]
    fn each_profile_checks_decoded_native_pixels_and_checked_visual_reservations() {
        for profile in [
            ImageProfile::AnthropicOpus55HighPatch28V1,
            ImageProfile::AnthropicSonnet55HighPatch28V1,
        ] {
            for (width, height, tokens) in [(1920, 1080, 3404_u32), (2576, 1456, 6020)] {
                let request = request(profile, &pixels(width, height), 1);
                let result = estimate_payload(caps(profile), &request, &json!({})).unwrap();
                assert_eq!(result.input_tokens, tokens.saturating_add(2));
                assert!(result.fits(caps(profile), &request));
            }
            for (width, height) in [(2577, 1), (2576, 1457), (2000, 2000)] {
                let request = request(profile, &pixels(width, height), 1);
                assert!(estimate_payload(caps(profile), &request, &json!({})).is_err());
            }
        }
        let profile = ImageProfile::OpenAiAstraHighPatch32V1;
        let good = request(profile, &pixels(1024, 1024), 1);
        assert_eq!(
            estimate_payload(caps(profile), &good, &json!({}))
                .unwrap()
                .input_tokens,
            1579
        );
        let bad = request(profile, &pixels(1025, 1), 1);
        assert!(estimate_payload(caps(profile), &bad, &json!({})).is_err());
    }

    #[test]
    fn output_reasoning_schemas_framing_and_request_bounds_cannot_bypass_preflight() {
        let profile = ImageProfile::OpenAiAstraHighPatch32V1;
        let mut request = request(profile, &pixels(64, 64), 8);
        let payload = json!({"messages": [{"role":"user", "content": "label"}],
            "tools": [{"description": "schema"}]});
        let estimate = estimate_payload(caps(profile), &request, &payload).unwrap();
        assert_eq!(estimate.images, 8);
        assert!(estimate.fits(caps(profile), &request));
        request.context_budget = Some(estimate.input_tokens.saturating_add(8192));
        assert!(estimate.fits(caps(profile), &request));
        request.separate_reasoning_tokens = 1;
        let next = estimate_payload(caps(profile), &request, &payload).unwrap();
        assert!(!next.fits(caps(profile), &request));
        request.max_tokens = Some(128_001);
        assert!(estimate_payload(caps(profile), &request, &payload).is_err());
        request.max_tokens = None;
        assert!(estimate_payload(caps(profile), &request, &payload).is_err());
        request.max_tokens = Some(8192);
        request.context_budget = None;
        request.messages.push(request.messages[1].clone());
        assert!(
            !estimate_payload(caps(profile), &request, &payload)
                .unwrap()
                .fits(caps(profile), &request)
        );
        request.messages.pop();
        let enormous = json!({ "text": "x".repeat(nanus_domain::content::RECORD_BYTES_MAX) });
        assert!(
            !estimate_payload(caps(profile), &request, &enormous)
                .unwrap()
                .fits(caps(profile), &request)
        );
        let schema = json!({"tools": ["schema".repeat(100)]});
        assert!(
            estimate_payload(caps(profile), &request, &schema)
                .unwrap()
                .input_tokens
                > estimate.input_tokens
        );
        let wrong = ModelCapabilities {
            max_input_tokens: Some(1),
            ..caps(profile)
        };
        assert!(!estimate.fits(wrong, &request));
        assert!(estimate_payload(ModelCapabilities::default(), &request, &payload).is_err());
    }
}
