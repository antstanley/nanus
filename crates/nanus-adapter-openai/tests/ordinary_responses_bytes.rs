//! The ordinary Responses bytes, pinned: a request that is not managed encodes its system prompt
//! as a leading developer item rather than `instructions`, and otherwise exactly as it did before
//! managed context learned the Responses wire. With caching asked for, the API plan marks the
//! prompt's part and the subscription marks nothing, so any drift in the shared encoder fails here.
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
