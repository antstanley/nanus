//! The actual minimal runner keeps immutable source while fitting original complete image turns.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use base64::Engine as _;
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, ContentBlock, Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId,
    ToolName, ToolRegistry,
};
use nanus_ports::{
    ChatRequest, ClockPort, FinishReason, ImageInputSupport, ImageProfile, LlmEvent, LlmPort,
    LlmStream, ModelCapabilities,
};
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        123
    }
}
struct Model(Rc<RefCell<Vec<ChatRequest>>>);
impl LlmPort for Model {
    fn model(&self) -> &'static str {
        "claude-sonnet-5-5"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities {
            image_input: ImageInputSupport::Supported,
            image_profile: Some(ImageProfile::AnthropicSonnet55HighPatch28V1),
            context_window_tokens: Some(1_000_000),
            max_input_tokens: Some(1_000_000),
            max_output_tokens: Some(128_000),
        }
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.0.borrow_mut().push(request);
        Box::pin(futures::stream::iter([
            LlmEvent::TextDelta("done".into()),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]))
    }
    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        let caps = self.capabilities(&request.model);
        // Match concrete provider preflight: its full-request image cap must apply to a fitted
        // candidate, not to the complete durable history before fitting can begin.
        nanus_ports::capabilities::validate_image_input(caps, request)?;
        let body = json!({"model":request.model,"messages":request.messages,
            "tools":request.tools,"max_tokens":request.max_tokens});
        nanus_ports::capabilities::estimate_payload(caps, request, &body)
    }
}
fn history() -> Session {
    let mut session = Session::new(SessionId::new("fictional-history"), 123, "/fictional");
    let pixels = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
    for index in 0..9 {
        let id = ToolCallId::new(format!("call-{index}"));
        session.append(SessionEvent::UserMessage {
            text: format!("old question {index}"),
        });
        session.append(SessionEvent::AssistantMessage {
            text: None,
            reasoning: None,
            replay: None,
            tool_calls: vec![ToolCall::new(
                id.clone(),
                ToolName::new("read_image").unwrap(),
                json!({}),
            )],
            usage: None,
            interrupted: false,
            model: Some("claude-sonnet-5-5".into()),
            effort: None,
        });
        session.append(SessionEvent::ToolResult {
            call_id: id,
            content: "fictional image".into(),
            content_blocks: Some(vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(pixels),
            }]),
            is_error: false,
        });
    }
    session
}
fn runner(requests: Rc<RefCell<Vec<ChatRequest>>>, budget: u32) -> AgentRunner {
    AgentRunner::new(
        Rc::new(Box::new(Model(requests))),
        ToolRegistryHandle::new(ToolRegistry::new()),
        "Fictional instructions",
        AgentConfig::new(4, 2, "claude-sonnet-5-5", 16384)
            .unwrap()
            .with_context_budget(budget)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(8192, 0)
}

#[tokio::test]
async fn nine_original_images_fit_after_whole_turn_elision_without_deleting_the_durable_source() {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let runner = runner(Rc::clone(&requests), 900_000);
    let mut session = history();
    let original = session.log().events().to_vec();
    assert!(
        runner
            .run_turn(&mut session, "latest", &mut Silent, None)
            .await
            .unwrap()
            .is_success()
    );
    let request = &requests.borrow()[0];
    let source = request
        .source_history
        .as_ref()
        .expect("original source retained");
    let source_images = source
        .iter()
        .filter(|message| {
            matches!(
                message,
                Message::Tool {
                    content_blocks: Some(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(source_images, 9);
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|message| matches!(
                message,
                Message::Tool {
                    content_blocks: Some(_),
                    ..
                }
            ))
            .count(),
        8
    );
    let projection =
        nanus_domain::context::identify_projection(source, &request.messages, 900_000).unwrap();
    assert_eq!(
        (projection.dropped_turns, projection.dropped_messages),
        (1, 3)
    );
    assert_eq!(&session.log().events()[..original.len()], &original);
    assert_eq!(
        Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap(),
        session
    );
}

#[tokio::test]
async fn an_invalid_original_image_is_not_hidden_by_fitting_and_does_not_reach_dispatch() {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let runner = runner(Rc::clone(&requests), 900_000);
    let mut session = history();
    // Keep a malformed original observation in memory: boundary validation must reject it even
    // if the oldest image turn could otherwise be dropped. No serialized fixture can encode it.
    session.append(SessionEvent::UserMessage {
        text: "another old question".into(),
    });
    let id = ToolCallId::new("malformed");
    session.append(SessionEvent::AssistantMessage {
        text: None,
        reasoning: None,
        replay: None,
        tool_calls: vec![ToolCall::new(
            id.clone(),
            ToolName::new("read_image").unwrap(),
            json!({}),
        )],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: id,
        content: "bad pixels".into(),
        is_error: false,
        content_blocks: Some(vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: "invalid".into(),
        }]),
    });
    assert!(
        runner
            .run_turn(&mut session, "latest", &mut Silent, None)
            .await
            .is_err()
    );
    assert!(requests.borrow().is_empty());
}
