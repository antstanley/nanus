//! The ordinary Responses bytes, pinned: a request that is not managed encodes its system prompt
//! as a leading developer item rather than `instructions`, and otherwise exactly as it did before
//! managed context learned the Responses wire. With caching asked for, the API plan marks the
//! prompt's part and the subscription marks nothing, and both send the session's cache key, so any
//! drift in the shared encoder fails here.
#![allow(clippy::unwrap_used)]

use nanus_adapter_openai::{
    OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, Vendor,
};
use nanus_domain::Message;
use nanus_ports::{ChatRequest, PromptCacheRetention};

const API_GOLDEN: &str = r#"{"input":[{"content":[{"text":"Be brief.","type":"input_text"}],"role":"developer"},{"content":[{"text":"Summarise.","type":"input_text"}],"role":"user"}],"max_output_tokens":128000,"model":"gpt-5.6-sol","reasoning":{"effort":"medium"},"store":false,"stream":true}"#;
const SUBSCRIPTION_GOLDEN: &str = r#"{"input":[{"content":[{"text":"Be brief.","type":"input_text"}],"role":"developer"},{"content":[{"text":"Summarise.","type":"input_text"}],"role":"user"}],"model":"gpt-5.6-sol","reasoning":{"effort":"medium"},"store":false,"stream":true}"#;
const API_CACHED_GOLDEN: &str = r#"{"input":[{"content":[{"prompt_cache_breakpoint":{"mode":"explicit"},"text":"Be brief.","type":"input_text"}],"role":"developer"},{"content":[{"text":"Summarise.","type":"input_text"}],"role":"user"}],"max_output_tokens":128000,"model":"gpt-5.6-sol","reasoning":{"effort":"medium"},"store":false,"stream":true}"#;

fn request() -> ChatRequest {
    ChatRequest::new(
        "gpt-5.6-sol",
        vec![Message::system("Be brief."), Message::user("Summarise.")],
    )
}

fn plans() -> (OpenAiLlm, OpenAiLlm) {
    let api = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5.6-sol", "test-key")).unwrap();
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-5.6-sol",
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    (api, OpenAiLlm::new(config).unwrap())
}

#[test]
fn the_ordinary_responses_bytes_on_both_plans_carry_the_prompt_as_a_developer_item() {
    let (api, subscription) = plans();
    let api_bytes = serde_json::to_string(&api.encode(&request())).unwrap();
    let subscription_bytes = serde_json::to_string(&subscription.encode(&request())).unwrap();
    assert_eq!(api_bytes, API_GOLDEN);
    assert_eq!(subscription_bytes, SUBSCRIPTION_GOLDEN);
}

#[test]
fn only_the_api_plan_marks_the_prompt_when_caching_is_asked_for() {
    let (api, subscription) = plans();
    let cached = request().with_prompt_cache(PromptCacheRetention::Short);
    let api_bytes = serde_json::to_string(&api.encode(&cached)).unwrap();
    let subscription_bytes = serde_json::to_string(&subscription.encode(&cached)).unwrap();
    assert_eq!(api_bytes, API_CACHED_GOLDEN);
    assert_eq!(subscription_bytes, SUBSCRIPTION_GOLDEN);
    assert!(!api_bytes.contains("prompt_cache_key"));
    assert!(!api_bytes.contains("prompt_cache_options"));
}

/// The cache key the runner names a session's requests with goes to both plans alike, beside the
/// API plan's breakpoint; asked for no caching, neither plan sends it.
#[test]
fn both_plans_send_the_session_cache_key_when_caching_is_asked_for() {
    let (api, subscription) = plans();
    let keyed = request()
        .with_prompt_cache(PromptCacheRetention::Short)
        .with_prompt_cache_key("0192f1c4-7d3e-7a10-9b2c-5d6e7f809a1b");
    let api_bytes = serde_json::to_string(&api.encode(&keyed)).unwrap();
    let subscription_bytes = serde_json::to_string(&subscription.encode(&keyed)).unwrap();
    let key = r#""model":"gpt-5.6-sol","prompt_cache_key":"0192f1c4-7d3e-7a10-9b2c-5d6e7f809a1b","#;
    let unkeyed = |golden: &str| golden.replace(key, r#""model":"gpt-5.6-sol","#);
    assert!(api_bytes.contains(key), "{api_bytes}");
    assert!(subscription_bytes.contains(key), "{subscription_bytes}");
    assert_eq!(unkeyed(&api_bytes), API_CACHED_GOLDEN);
    assert_eq!(unkeyed(&subscription_bytes), SUBSCRIPTION_GOLDEN);

    let unasked = keyed.with_prompt_cache(PromptCacheRetention::None);
    assert_eq!(
        serde_json::to_string(&api.encode(&unasked)).unwrap(),
        API_GOLDEN
    );
    assert_eq!(
        serde_json::to_string(&subscription.encode(&unasked)).unwrap(),
        SUBSCRIPTION_GOLDEN
    );
}
