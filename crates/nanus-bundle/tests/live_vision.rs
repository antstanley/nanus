//! Live vision evidence: the exact candidate models answer about a tool-result image, then
//! answer a follow-up that refers back to the call that produced it.
//!
//! These spend API credit and send a tiny fictional fixture (a green triangle on white) to the
//! provider, so they are `#[ignore]`d. Run them deliberately:
//!
//! ```sh
//! cargo nextest run -p nanus-bundle --test live_vision --run-ignored ignored-only
//! ```
//!
//! Credentials are read through the same store chain a run uses and are never printed. The body
//! sent is the one the adapter's own `encode` produces for the session, so this proves the
//! encoded wire and not a hand-built imitation. A pass is evidence for a promotion; it is not
//! the promotion, which is a separate edit with the evidence recorded beside it.
#![allow(clippy::unwrap_used, clippy::print_stderr)]

use base64::Engine as _;
use nanus_domain::{
    ContentBlock, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName,
};
use nanus_ports::ChatRequest;
use serde_json::{Value, json};

const PNG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
const JPEG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.jpg");
/// The argument that tells the two calls apart. A call id is a protocol field some providers never
/// show the model, so the follow-up asks about something the model can actually read.
const LABEL: &str = "alpha";

/// The PNG fixture re-encoded as lossless WebP, so every accepted format is exercised live.
fn webp() -> Vec<u8> {
    let pixels = image::load_from_memory_with_format(PNG, image::ImageFormat::Png).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut out, image::ImageFormat::WebP).unwrap();
    out.into_inner()
}

/// The PNG fixture re-encoded as a single-frame GIF.
fn gif() -> Vec<u8> {
    let pixels = image::load_from_memory_with_format(PNG, image::ImageFormat::Png).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut out, image::ImageFormat::Gif).unwrap();
    out.into_inner()
}

fn session(media: &str, bytes: &[u8], model: &str) -> Session {
    let mut session = Session::new(SessionId::new("live-vision"), 1, "/live");
    session.append(SessionEvent::UserMessage {
        content_blocks: None,
        text: "Two inspect calls ran; one returned an image. Look at that image. What shape is it and what colour? \
               Answer in one short sentence."
            .into(),
    });
    let call = |id: &str, label: &str| {
        ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("inspect").unwrap(),
            json!({ "label": label }),
        )
    };
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![call("call-a", LABEL), call("call-b", "beta")],
        usage: None,
        interrupted: false,
        model: Some(model.into()),
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("call-a"),
        content: "image".into(),
        content_blocks: Some(vec![
            ContentBlock::Text("fixture".into()),
            ContentBlock::Image {
                media_type: media.into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        ]),
        is_error: false,
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("call-b"),
        content: "nothing to show".into(),
        content_blocks: None,
        is_error: false,
    });
    session
}

fn follow_up(mut session: Session, answer: &str, model: &str) -> Session {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some(answer.to_owned()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: Some(model.into()),
        effort: None,
    });
    session.append(SessionEvent::UserMessage {
        content_blocks: None,
        text: "Which of the two inspect calls returned that image? Reply with only the value of its `label` argument."
            .into(),
    });
    session
}

fn formats() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("image/png", PNG.to_vec()),
        ("image/jpeg", JPEG.to_vec()),
        ("image/webp", webp()),
        ("image/gif", gif()),
    ]
}

async fn key(account: &str) -> String {
    let secrets = nanus_bundle::compose::open_secrets().unwrap();
    let secret = secrets.get(account).await.unwrap().unwrap();
    assert!(!secret.is_blank(), "no credential stored for {account}");
    secret.expose().to_owned()
}

/// The answer text of a non-streamed response, whichever API shaped it: chat completions
/// (`choices`), Anthropic (`content` blocks) or `OpenAI` Responses (`output` items, each a message
/// whose content carries `output_text` parts; reasoning items have none).
fn extract_answer(value: &Value) -> String {
    if let Some(content) = value["choices"][0]["message"]["content"].as_str() {
        return content.to_owned();
    }
    if let Some(blocks) = value["content"].as_array() {
        return blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
    }
    value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Posts a non-streamed copy of `body` and returns the answer text, or fails with the API's own
/// error body (which never contains the credential).
async fn post(url: &str, headers: &[(&str, String)], mut body: Value) -> String {
    body["stream"] = json!(false);
    // Only valid on a stream.
    body.as_object_mut().unwrap().remove("stream_options");
    let mut request = reqwest::Client::new().post(url).json(&body);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(status.is_success(), "{url} answered {status}: {text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    let answer = extract_answer(&value);
    assert!(
        !answer.trim().is_empty(),
        "an empty answer from {url}: {text}"
    );
    answer
}

fn assert_shape(answer: &str, model: &str, media: &str) {
    let lower = answer.to_lowercase();
    eprintln!("[{model} {media}] shape answer: {answer}");
    assert!(
        lower.contains("triangle") && lower.contains("green"),
        "{model} {media} did not describe a green triangle: {answer}"
    );
}

fn assert_reference(answer: &str, model: &str, media: &str) {
    eprintln!("[{model} {media}] call-reference answer: {answer}");
    assert!(
        answer.to_lowercase().contains(LABEL),
        "{model} {media} did not name the producing call {LABEL}: {answer}"
    );
}

async fn anthropic(model: &str) {
    let api_key = key("anthropic").await;
    let llm = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::new(model, api_key.clone()),
    )
    .unwrap();
    let headers = [
        ("x-api-key", api_key),
        (
            "anthropic-version",
            nanus_adapter_anthropic::API_VERSION.to_owned(),
        ),
    ];
    for (media, bytes) in formats() {
        let first = session(media, &bytes, model);
        let mut body = llm.encode(&ChatRequest::new(model, first.derive_messages()));
        body["max_tokens"] = json!(8000);
        let answer = post(&llm.endpoint(), &headers, body).await;
        assert_shape(&answer, model, media);
        let second = follow_up(first, &answer, model);
        let mut body = llm.encode(&ChatRequest::new(model, second.derive_messages()));
        body["max_tokens"] = json!(8000);
        let reference = post(&llm.endpoint(), &headers, body).await;
        assert_reference(&reference, model, media);
    }
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn claude_opus_5_5_reads_a_tool_result_image_and_follows_the_call() {
    anthropic("claude-opus-5-5").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn claude_sonnet_5_5_reads_a_tool_result_image_and_follows_the_call() {
    anthropic("claude-sonnet-5-5").await;
}

async fn openai(model: &str) {
    let api_key = key("openai").await;
    let llm = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        model,
        api_key.clone(),
    ))
    .unwrap();
    // `gpt-5.6` and later are Responses-first (chat completions refuses function tools beside an
    // effort), so the Responses endpoint is the one under test.
    let url = format!(
        "{}{}",
        nanus_adapter_openai::OPENAI_BASE_URL.trim_end_matches('/'),
        nanus_adapter_openai::Protocol::Responses.path()
    );
    let headers = [("authorization", format!("Bearer {api_key}"))];
    for (media, bytes) in formats() {
        let first = session(media, &bytes, model);
        let body = llm.encode(&ChatRequest::new(model, first.derive_messages()));
        assert!(body.get("input").is_some(), "{body}");
        let answer = post(&url, &headers, body).await;
        assert_shape(&answer, model, media);
        let second = follow_up(first, &answer, model);
        let body = llm.encode(&ChatRequest::new(model, second.derive_messages()));
        let reference = post(&url, &headers, body).await;
        assert_reference(&reference, model, media);
    }
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_6_astra_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-6-astra").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_6_1_sol_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-6.1-sol").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_6_luna_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-6-luna").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_5_6_sol_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-5.6-sol").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_5_6_terra_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-5.6-terra").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn gpt_5_6_luna_reads_a_tool_result_image_and_follows_the_call() {
    openai("gpt-5.6-luna").await;
}

/// Posts `body` as a stream to the `ChatGPT` backend, which only answers streams, and returns the
/// concatenated output text. A failure carries the backend's own error body.
async fn post_stream(url: &str, headers: &[(&str, String)], body: Value) -> String {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("accept", "text/event-stream")
        .json(&body);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(status.is_success(), "{url} answered {status}: {text}");
    let mut answer = String::new();
    let mut failure = None;
    for payload in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        match event["type"].as_str() {
            Some("response.output_text.delta") => {
                answer.push_str(event["delta"].as_str().unwrap_or_default());
            }
            Some("response.failed" | "error") => failure = Some(event),
            _ => {}
        }
    }
    assert!(
        failure.is_none(),
        "the backend reported a failure: {failure:?}"
    );
    assert!(!answer.trim().is_empty(), "an empty answer: {text}");
    answer
}

/// The `ChatGPT` subscription is a different endpoint from `api.openai.com`, so it needs evidence of
/// its own. The authorization is read through the credential chain, and must be current: run
/// `nanus run` once first if it has expired, which renews and stores it.
async fn chatgpt_subscription(model: &str) {
    let stored = key("openai:subscription").await;
    let tokens = nanus_adapter_openai::oauth::Tokens::decode(&stored).unwrap();
    assert!(
        !tokens.is_expired(std::time::Duration::from_mins(1)),
        "the authorization has expired; run `nanus run` once to renew it"
    );
    let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        nanus_adapter_openai::Vendor::OpenAi,
        model,
        tokens.access_token.clone(),
        "https://chatgpt.com/backend-api/codex",
    );
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    let mut headers = vec![("authorization", format!("Bearer {}", tokens.access_token))];
    if let Some(account) = tokens.account_id.clone() {
        config.set_account_id(account.clone());
        headers.push(("chatgpt-account-id", account));
    }
    let llm = nanus_adapter_openai::OpenAiLlm::new(config).unwrap();
    let url = llm.endpoint();
    assert!(url.ends_with("/responses"), "{url}");
    for (media, bytes) in formats() {
        let first = session(media, &bytes, model);
        let body = llm.encode(&ChatRequest::new(model, first.derive_messages()));
        assert!(body.get("max_output_tokens").is_none(), "{body}");
        let answer = post_stream(&url, &headers, body).await;
        assert_shape(&answer, model, media);
        let second = follow_up(first, &answer, model);
        let body = llm.encode(&ChatRequest::new(model, second.derive_messages()));
        let reference = post_stream(&url, &headers, body).await;
        assert_reference(&reference, model, media);
    }
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_6_astra_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-6-astra").await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_6_1_sol_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-6.1-sol").await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_6_luna_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-6-luna").await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_5_6_sol_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-5.6-sol").await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_5_6_terra_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-5.6-terra").await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn chatgpt_gpt_5_6_luna_reads_a_tool_result_image_and_follows_the_call() {
    chatgpt_subscription("gpt-5.6-luna").await;
}

/// `deepseek-flash` on the chat-completions endpoint, through the adapter's own encoding.
async fn deepseek(model: &str) {
    let api_key = key("deepseek").await;
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new(model, api_key.clone()),
    )
    .unwrap();
    let headers = [("authorization", format!("Bearer {api_key}"))];
    for (media, bytes) in formats() {
        let first = session(media, &bytes, model);
        let mut body = llm.encode(&ChatRequest::new(model, first.derive_messages()));
        body["max_tokens"] = json!(8000);
        let answer = post(&llm.endpoint(), &headers, body).await;
        assert_shape(&answer, model, media);
        let second = follow_up(first, &answer, model);
        let mut body = llm.encode(&ChatRequest::new(model, second.derive_messages()));
        body["max_tokens"] = json!(8000);
        let reference = post(&llm.endpoint(), &headers, body).await;
        assert_reference(&reference, model, media);
    }
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn deepseek_flash_reads_a_tool_result_image_and_follows_the_call() {
    deepseek("deepseek-flash").await;
}

/// `deepseek-v4-pro` is recorded as not taking images. This asks the API directly, so the record
/// is evidence rather than an assumption: the request must be refused, or answered without having
/// seen the picture. If it ever starts describing the triangle this fails, and the model is a
/// candidate for a profile of its own.
#[tokio::test]
#[ignore = "live: spends API credit"]
async fn deepseek_v4_pro_is_not_shown_to_read_images() {
    let model = "deepseek-v4-pro";
    let api_key = key("deepseek").await;
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new(model, api_key.clone()),
    )
    .unwrap();
    let mut body = llm.encode(&ChatRequest::new(
        model,
        session("image/png", PNG, model).derive_messages(),
    ));
    body["max_tokens"] = json!(8000);
    body["stream"] = json!(false);
    // Only valid on a stream.
    body.as_object_mut().unwrap().remove("stream_options");
    let response = reqwest::Client::new()
        .post(llm.endpoint())
        .header("authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    eprintln!(
        "[{model}] status {status}: {}",
        &text[..text.len().min(400)]
    );
    if status.is_success() {
        let answer = extract_answer(&serde_json::from_str(&text).unwrap()).to_lowercase();
        eprintln!("[{model}] answer: {}", &answer[..answer.len().min(300)]);
        assert!(
            !(answer.contains("triangle") && answer.contains("green")),
            "{model} described the picture; it may take images now: {answer}"
        );
    }
}

/// What `deepseek-flash` charges, in prompt tokens, for an image of a given size.
///
/// `DeepSeek` documents the limits of an image but not its price, so the profile's reservation has
/// to rest on a measurement: each size is sent once beside a text-only twin of the same request,
/// and the difference in `usage.prompt_tokens` is what the picture cost. Prints a table; it
/// asserts only that the cost is positive and grows with the area, which is what a reservation
/// formula can lean on.
#[tokio::test]
#[ignore = "live: spends API credit"]
async fn deepseek_flash_image_token_cost_by_size() {
    let model = "deepseek-flash";
    let api_key = key("deepseek").await;
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new(model, api_key.clone()),
    )
    .unwrap();
    let headers = [("authorization", format!("Bearer {api_key}"))];
    let prompt_tokens = |value: &Value| value["usage"]["prompt_tokens"].as_u64().unwrap();
    let ask = |body: Value| {
        let headers = &headers;
        let url = llm.endpoint();
        async move {
            let mut body = body;
            body["stream"] = json!(false);
            body.as_object_mut().unwrap().remove("stream_options");
            body["max_tokens"] = json!(16);
            // Thinking off for both twins: with it on, a tool result that directly continues an
            // assistant turn is refused without that turn's reasoning, and this synthetic session
            // has none to give. The two requests must differ only by the picture.
            body["thinking"] = json!({ "type": "disabled" });
            body.as_object_mut().unwrap().remove("reasoning_effort");
            let response = reqwest::Client::new()
                .post(url)
                .header("authorization", headers[0].1.clone())
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert!(status.is_success(), "answered {status}: {text}");
            serde_json::from_str::<Value>(&text).unwrap()
        }
    };
    let mut costs = Vec::new();
    for (width, height) in [
        (28, 28),
        (256, 256),
        (640, 360),
        (1024, 576),
        (1024, 1024),
        (2000, 1000),
    ] {
        // Smooth noise so the encoder cannot collapse the picture into nothing.
        let pixels = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                u8::try_from((x * 7 + y * 3) % 256).unwrap(),
                u8::try_from((x * 5 + y * 11) % 256).unwrap(),
                u8::try_from((x + y * 13) % 256).unwrap(),
            ])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        pixels.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let with_image = session("image/png", png.get_ref(), model);
        let mut twin = session("image/png", png.get_ref(), model);
        // The text-only twin: the same session with the picture's result replaced by text.
        twin = {
            let mut text_only = Session::new(SessionId::new("calibration-twin"), 1, "/live");
            for event in twin.log().events() {
                let mut event = event.clone();
                if let SessionEvent::ToolResult { content_blocks, .. } = &mut event {
                    *content_blocks = None;
                }
                text_only.append(event);
            }
            text_only
        };
        let image_body = llm.encode(&ChatRequest::new(model, with_image.derive_messages()));
        let text_body = llm.encode(&ChatRequest::new(model, twin.derive_messages()));
        eprintln!("[calibration {width}x{height}] sending the image request");
        let with = prompt_tokens(&ask(image_body).await);
        eprintln!("[calibration {width}x{height}] sending the text-only twin");
        let without = prompt_tokens(&ask(text_body).await);
        let cost = with.saturating_sub(without);
        eprintln!(
            "[deepseek-flash {width}x{height}] image cost {cost} prompt tokens ({with} vs {without})"
        );
        costs.push((u64::from(width) * u64::from(height), cost));
    }
    assert!(costs.iter().all(|(_, cost)| *cost > 0), "{costs:?}");
    assert!(
        costs.windows(2).all(|pair| pair[1].1 >= pair[0].1),
        "the cost does not fall as the area grows: {costs:?}"
    );
}
