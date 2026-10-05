use super::*;
use base64::Engine as _;
use nanus_domain::ContentBlock;

#[test]
fn actual_original_item_encoder_replaces_png_and_jpeg_payload_tokens_with_visual_cost() {
    let mut config = config();
    config.set_stateless_responses(true);
    let adapter = OpenAiLlm::new(config).unwrap();
    let mut request = continuation(&adapter);
    let profile = adapter
        .capabilities(&request.model)
        .require_image_profile(&request.model)
        .unwrap();
    let mut visual = 0_u32;
    for (index, media, bytes) in [
        (
            3,
            "image/png",
            include_bytes!("../../../nanus-domain/tests/data/tiny-green-triangle.png").as_slice(),
        ),
        (
            4,
            "image/jpeg",
            include_bytes!("../../../nanus-domain/tests/data/tiny-green-triangle.jpg").as_slice(),
        ),
    ] {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let dimensions = nanus_domain::content::validate_image(media, &data).unwrap();
        visual = visual
            .checked_add(profile.reserved_tokens(dimensions).unwrap())
            .unwrap();
        let Message::Tool { content_blocks, .. } = &mut request.messages[index] else {
            unreachable!()
        };
        *content_blocks = Some(vec![ContentBlock::Image {
            media_type: media.into(),
            data_base64: data,
        }]);
    }
    let prepared = adapter.prepare_responses(&request).unwrap();
    let estimate = adapter.estimate_request(&request).unwrap();
    let input = prepared.body["input"].as_array().unwrap();
    let urls: Vec<_> = input
        .iter()
        .filter_map(|v| v["content"].as_array())
        .flatten()
        .filter(|v| v["type"] == "input_image")
        .map(|v| v["image_url"].as_str().unwrap())
        .collect();
    assert_eq!(urls.len(), 2);
    let pixels = urls.iter().map(|s| s.len()).sum::<usize>();
    let wire = serde_json::to_vec(&prepared.body).unwrap().len();
    assert_eq!(estimate.request_bytes, wire);
    assert_eq!(
        estimate.input_tokens,
        u32::try_from(wire.checked_sub(pixels).unwrap())
            .unwrap()
            .checked_add(visual)
            .unwrap()
    );
    assert_eq!(estimate.images, 2);
    assert!(estimate.fits(adapter.capabilities(&request.model), &request));
}
