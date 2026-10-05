use super::*;
use base64::Engine as _;
use serde_json::{Value, json};

fn image(format: image::ImageFormat, noisy: bool) -> ContentBlock {
    let mut image = image::RgbImage::new(64, 64);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let n = if noisy {
            x.saturating_mul(31).saturating_add(y.saturating_mul(73))
        } else {
            0
        };
        *pixel = image::Rgb([u8::try_from(n % 256).unwrap(), 23, 79]);
    }
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, format)
        .unwrap();
    ContentBlock::Image {
        media_type: if format == image::ImageFormat::Png {
            "image/png"
        } else {
            "image/jpeg"
        }
        .into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(output.into_inner()),
    }
}

fn body(block: &ContentBlock) -> Value {
    let ContentBlock::Image {
        media_type,
        data_base64,
    } = block
    else {
        unreachable!()
    };
    json!({"input":[{"role":"user","content":[
        {"type":"input_text","text":"original attachment label"},
        {"type":"input_image","image_url":format!("data:{media_type};base64,{data_base64}"),
        "detail":"high"}]}],"max_output_tokens":8192})
}

#[test]
fn responses_images_charge_original_wire_bytes_and_visual_tokens_separately() {
    let profile = ImageProfile::OpenAiAstraHighPatch32V1;
    for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
        let mut estimates = Vec::new();
        for noisy in [false, true] {
            let block = image(format, noisy);
            let request = tests::request(profile, &block, 1);
            let payload = body(&block);
            let estimate = estimate_payload(tests::caps(profile), &request, &payload).unwrap();
            let wire = serde_json::to_vec(&payload).unwrap().len();
            let data = payload["input"][0]["content"][1]["image_url"]
                .as_str()
                .unwrap();
            let visual = profile
                .reserved_tokens(nanus_domain::content::ImageDimensions {
                    width: 64,
                    height: 64,
                })
                .unwrap();
            assert_eq!(estimate.request_bytes, wire);
            assert_eq!(
                estimate.input_tokens,
                u32::try_from(wire.checked_sub(data.len()).unwrap())
                    .unwrap()
                    .checked_add(visual)
                    .unwrap()
            );
            assert_eq!(estimate.images, 1);
            estimates.push(estimate);
        }
        assert_ne!(estimates[0].request_bytes, estimates[1].request_bytes);
        assert_eq!(estimates[0].input_tokens, estimates[1].input_tokens);
    }
}

#[test]
fn responses_lookalikes_in_schemas_calls_outputs_and_text_keep_their_entire_cost() {
    let profile = ImageProfile::OpenAiAstraHighPatch32V1;
    let block = image(image::ImageFormat::Png, false);
    let request = tests::request(profile, &block, 1);
    let payload = body(&block);
    let image = payload["input"][0]["content"][1].clone();
    let lookalikes = [
        json!({"type":"function_call","name":"fictional","role":"user",
            "content":[image],"arguments":{"image":image}}),
        json!({"type":"function_call_output","role":"user","content":[image]}),
        json!({"role":"assistant","content":[image]}),
        json!({"role":"user","content":[{"type":"input_text","text":image.to_string(),
            "image_url":image["image_url"]}]}),
    ];
    for fake in lookalikes {
        for field in ["tools", "input"] {
            let mut changed = payload.clone();
            if field == "tools" {
                changed[field] = json!([fake]);
            } else {
                changed[field].as_array_mut().unwrap().push(fake.clone());
            }
            assert_text_growth(&request, &payload, &changed);
        }
    }
}

fn assert_text_growth(request: &ChatRequest, original: &Value, changed: &Value) {
    let caps = tests::caps(ImageProfile::OpenAiAstraHighPatch32V1);
    let base = estimate_payload(caps, request, original).unwrap();
    let measured = estimate_payload(caps, request, changed).unwrap();
    let growth = measured
        .request_bytes
        .checked_sub(base.request_bytes)
        .unwrap();
    assert_eq!(
        measured
            .input_tokens
            .checked_sub(base.input_tokens)
            .unwrap(),
        u32::try_from(growth).unwrap()
    );
    assert_eq!(measured.images, base.images);
}
