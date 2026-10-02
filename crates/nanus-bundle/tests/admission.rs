//! Image admission: a batch of tool results that cannot fit the next request is refused before
//! any of it runs, and the refused call's work never starts.
//!
//! The counterexample the design names: three calls that may each return four images, with room
//! for eight in a request. The first two are admitted, the third is answered with a bounded
//! failure and its executor is not invoked.

// Fixture helpers use panics only as assertions.
#![allow(clippy::unwrap_used)]
#![cfg(test)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use base64::Engine as _;
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, ContentBlock, Session, SessionEvent, SessionId, ToolCall, ToolCallId,
    ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolOutcome, ToolRegistry, ToolResult,
    ToolSchema,
};
use nanus_ports::{
    ChatRequest, ClockPort, FinishReason, ImageInputSupport, ImageProfile, LlmEvent, LlmPort,
    LlmStream, ModelCapabilities,
};
use serde_json::json;

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        1
    }
}

/// A model that answers each request with the next scripted batch of events.
struct Scripted {
    responses: RefCell<Vec<Vec<LlmEvent>>>,
    profile: bool,
}

impl LlmPort for Scripted {
    fn model(&self) -> &'static str {
        "claude-sonnet-5-5"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        if !self.profile {
            return ModelCapabilities::default();
        }
        ModelCapabilities {
            image_input: ImageInputSupport::Supported,
            image_profile: Some(ImageProfile::AnthropicSonnet55HighPatch28V1),
            context_window_tokens: Some(1_000_000),
            max_input_tokens: Some(1_000_000),
            max_output_tokens: Some(128_000),
        }
    }
    fn stream_chat(&self, _: ChatRequest) -> LlmStream {
        let next = self.responses.borrow_mut().remove(0);
        Box::pin(futures::stream::iter(next))
    }
}

fn calls(count: usize) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    for index in 0..count {
        events.push(LlmEvent::ToolCallDelta {
            index: u32::try_from(index).unwrap(),
            id: Some(ToolCallId::new(format!("call-{index}"))),
            name: Some(ToolName::new("frames").unwrap()),
            arguments_delta: "{}".into(),
        });
    }
    events.push(LlmEvent::Finished {
        reason: FinishReason::ToolCalls,
    });
    events
}

fn answer() -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta("done".into()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

fn jpeg() -> String {
    let pixels = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 30]));
    let mut out = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    base64::engine::general_purpose::STANDARD.encode(out.into_inner())
}

/// Returns `images` images, and counts how often it ran.
struct Frames {
    images: usize,
    ran: Rc<Cell<u32>>,
}

impl ToolExecutor for Frames {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        self.ran.set(self.ran.get().saturating_add(1));
        let blocks: Vec<ContentBlock> = (0..self.images)
            .map(|_| ContentBlock::Image {
                media_type: "image/jpeg".into(),
                data_base64: jpeg(),
            })
            .collect();
        Box::pin(
            async move { ToolResult::new(call.id, ToolOutcome::success_with(json!({}), blocks)) },
        )
    }
}

/// A runner over one `frames` tool declared to return up to `declared` images of at most
/// `each_bytes`, which actually returns `actual`.
fn runner(
    script: Vec<Vec<LlmEvent>>,
    profile: bool,
    declared: Option<(u32, u32)>,
    actual: usize,
) -> (AgentRunner, Rc<Cell<u32>>) {
    let ran = Rc::new(Cell::new(0));
    let schema = ToolSchema {
        name: ToolName::new("frames").unwrap(),
        description: "returns frames for the admission test".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    };
    let mut tool = ToolDefinition::new(
        schema,
        Frames {
            images: actual,
            ran: Rc::clone(&ran),
        },
    )
    .with_access(nanus_domain::ToolAccess::Read);
    if let Some((count, bytes)) = declared {
        tool = tool.with_result_images(count, bytes);
    }
    let mut registry = ToolRegistry::new();
    registry.register(tool).unwrap();
    let runner = AgentRunner::new(
        Rc::new(Box::new(Scripted {
            responses: RefCell::new(script),
            profile,
        })),
        ToolRegistryHandle::new(registry),
        "host",
        AgentConfig::new(6, 4, "claude-sonnet-5-5", 4096)
            .unwrap()
            .with_context_budget(900_000)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(8192, 0);
    (runner, ran)
}

fn results(session: &Session) -> Vec<(String, bool, String)> {
    session
        .log()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResult {
                call_id,
                is_error,
                content,
                ..
            } => Some((call_id.as_str().to_owned(), *is_error, content.clone())),
            _ => None,
        })
        .collect()
}

fn session() -> Session {
    Session::new(SessionId::new("admission"), 0, "workspace")
}

const SMALL: u32 = 128 * 1024;

/// Three calls of up to four images each, room for eight: two are admitted and run, the third is
/// refused with a bounded failure before its executor starts.
#[tokio::test]
async fn the_third_four_image_call_is_refused_before_it_runs() {
    let (runner, ran) = runner(vec![calls(3), answer()], true, Some((4, SMALL)), 4);
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(ran.get(), 2, "the refused call's executor is never invoked");
    let results = results(&session);
    assert_eq!(results.len(), 3, "every call is answered, in order");
    assert_eq!(
        results.iter().map(|r| r.1).collect::<Vec<_>>(),
        [false, false, true]
    );
    assert!(results[2].2.contains("not run"), "{}", results[2].2);
    assert!(results[2].2.len() < 1024, "the failure slot is bounded");
}

/// Images retained from an earlier step of the same turn count against the next call.
#[tokio::test]
async fn images_already_held_by_the_turn_count_against_a_later_step() {
    let (runner, ran) = runner(
        vec![calls(1), calls(2), answer()],
        true,
        Some((4, SMALL)),
        4,
    );
    let mut session = session();
    runner
        .run_turn(&mut session, "look twice", &mut Silent, None)
        .await
        .unwrap();
    // Step one holds four images. Step two's first call fits (8), its second does not.
    assert_eq!(ran.get(), 2);
    let results = results(&session);
    assert_eq!(
        results.iter().map(|r| r.1).collect::<Vec<_>>(),
        [false, false, true]
    );
}

/// With the default 512 KiB envelope the byte cap refuses before the image cap does.
#[tokio::test]
async fn the_encoded_byte_cap_can_refuse_before_the_image_cap() {
    let (runner, ran) = runner(vec![calls(2), answer()], true, Some((4, 512 * 1024)), 1);
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(
        ran.get(),
        1,
        "four 512 KiB images twice is over the 4 MiB request cap"
    );
    assert!(results(&session)[1].2.contains("not run"));
}

/// A tool that declares an envelope is held to it, and one that declares none is untouched.
#[tokio::test]
async fn a_result_beyond_its_declared_envelope_is_replaced_and_an_undeclared_tool_is_not_bound() {
    let (over, _) = runner(vec![calls(1), answer()], true, Some((1, SMALL)), 3);
    let mut one = session();
    over.run_turn(&mut one, "go", &mut Silent, None)
        .await
        .unwrap();
    let got = results(&one);
    assert!(
        got[0].1 && got[0].2.contains("declared at most 1"),
        "{got:?}"
    );

    let (undeclared, ran) = runner(vec![calls(3), answer()], true, None, 2);
    let mut two = session();
    undeclared
        .run_turn(&mut two, "go", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(ran.get(), 3, "no declaration, no admission");
    assert!(results(&two).iter().all(|r| !r.1));
}

/// A model without a verified image profile is not admitted against: it cannot retain pixels.
#[tokio::test]
async fn a_model_without_a_profile_is_not_admitted_against() {
    let (runner, ran) = runner(vec![calls(3), answer()], false, Some((4, SMALL)), 0);
    let mut session = session();
    runner
        .run_turn(&mut session, "go", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(ran.get(), 3, "all three run: none of them returns pixels");
}
