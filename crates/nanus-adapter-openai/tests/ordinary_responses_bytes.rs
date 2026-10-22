//! The ordinary Responses bytes, pinned: a request that is not managed encodes exactly as it did
//! before managed context learned the Responses wire. The goldens were taken from the revision
//! before that change, so any drift in the shared encoder fails here.

use nanus_adapter_openai::{
    OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, Vendor,
};
use nanus_domain::Message;
use nanus_ports::ChatRequest;

const API_GOLDEN: &str = r#"{"input":[{"content":[{"text":"Summarise.","type":"input_text"}],"role":"user"}],"instructions":"Be brief.","max_output_tokens":128000,"model":"gpt-5.6-sol","reasoning":{"effort":"medium"},"store":false,"stream":true}"#;
const SUBSCRIPTION_GOLDEN: &str = r#"{"input":[{"content":[{"text":"Summarise.","type":"input_text"}],"role":"user"}],"instructions":"Be brief.","model":"gpt-5.6-sol","reasoning":{"effort":"medium"},"store":false,"stream":true}"#;

fn request() -> ChatRequest {
    ChatRequest::new(
        "gpt-5.6-sol",
        vec![Message::system("Be brief."), Message::user("Summarise.")],
    )
}

#[test]
fn the_ordinary_responses_bytes_on_both_plans_are_unchanged() {
    let api = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5.6-sol", "test-key")).unwrap();
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-5.6-sol",
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    let subscription = OpenAiLlm::new(config).unwrap();

    let api_bytes = serde_json::to_string(&api.encode(&request())).unwrap();
    let subscription_bytes = serde_json::to_string(&subscription.encode(&request())).unwrap();
    assert_eq!(api_bytes, API_GOLDEN);
    assert_eq!(subscription_bytes, SUBSCRIPTION_GOLDEN);
}
