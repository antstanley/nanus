//! Live Inception smoke test: the public endpoint accepts the body the adapter sends.
//!
//! This spends credit and needs a key, so it is `#[ignore]`d. Run it deliberately:
//!
//! ```sh
//! INCEPTION_API_KEY=... cargo nextest run -p nanus-adapter-openai \
//!     --test live_inception --run-ignored ignored-only
//! ```
//!
//! It asks `mercury-3` for one short answer at Inception's lowest accepted step (`minimal` on
//! the neutral scale, `instant` on the wire) and requires visible text, a usage report, and a
//! terminal reason. A pass proves the public endpoint accepts this adapter's request shape and
//! stream framing; it does not prove tool-call behavior or managed context.
#![allow(clippy::expect_used)]

use futures::StreamExt as _;
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Vendor};
use nanus_domain::Message;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort as _, ReasoningEffort};

#[tokio::test]
#[ignore = "live: spends Inception credit; set INCEPTION_API_KEY"]
async fn mercury_answers_a_text_turn_at_the_instant_step() {
    let config = OpenAiConfig::from_env(Vendor::Inception, "mercury-3")
        .expect("INCEPTION_API_KEY must be set to run this live test");
    let llm = OpenAiLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(
        "mercury-3",
        vec![Message::user("Reply with the single word: pong")],
    )
    .with_max_tokens(64)
    .with_reasoning_effort(ReasoningEffort::Minimal);

    let events: Vec<LlmEvent> = llm.stream_chat(request).collect().await;
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(!text.trim().is_empty(), "no visible answer: {events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Usage(_))),
        "no usage was reported: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(LlmEvent::Finished { .. })),
        "the stream did not end with a reason: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Error(_))),
        "a live error was reported: {events:?}"
    );
}
