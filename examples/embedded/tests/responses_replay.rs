//! Decoder-to-runner persistence proof; request-prefix admission/encoding is intentionally separate.
#![cfg(feature = "providers")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::RefCell;
use std::rc::Rc;

use nanus_adapter_openai::responses::StreamAccumulator;
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor};
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, Message, Session, SessionId, ToolCall, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolRegistry, ToolResult, ToolSchema,
};
use nanus_ports::{ChatRequest, ClockPort, LlmEvent, LlmPort, LlmStream, ResponseLimits};
use serde_json::{Value, json};

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        123
    }
}

struct Model {
    scripts: RefCell<Vec<Vec<Value>>>,
    requests: RefCell<Vec<ChatRequest>>,
    adapter: Option<OpenAiLlm>,
    bodies: RefCell<Vec<Value>>,
}
impl LlmPort for Model {
    fn model(&self) -> &str {
        "gpt-6-astra"
    }
    fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
        self.adapter
            .as_ref()
            .map_or_else(Default::default, |adapter| adapter.capabilities(model))
    }
    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        let body = self
            .adapter
            .as_ref()
            .map(|adapter| adapter.prepare_responses(request))
            .transpose()?
            .map(|prepared| prepared.body)
            .unwrap_or_else(|| json!({"fictional":true}));
        nanus_ports::capabilities::estimate_payload(
            self.capabilities(&request.model),
            request,
            &body,
        )
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        let prepared = self
            .adapter
            .as_ref()
            .map(|adapter| adapter.prepare_responses(&request).unwrap());
        self.requests.borrow_mut().push(request);
        let frames = self.scripts.borrow_mut().remove(0);
        // Fictional trusted prefix exercises propagation, not a fabricated HTTP admission proof.
        let limits = ResponseLimits::new(8192, 8192, 65536, 100, 256, 1024).unwrap();
        let mut accumulator = if let Some(prepared) = prepared {
            self.bodies.borrow_mut().push(prepared.body);
            StreamAccumulator::with_context(
                prepared.prefix_digest,
                prepared.context_receipt,
                limits,
            )
            .unwrap()
        } else {
            StreamAccumulator::with_prefix("a".repeat(64), limits).unwrap()
        };
        for frame in frames {
            accumulator.observe_frame(&frame);
        }
        accumulator.close();
        let events: Vec<LlmEvent> = std::iter::from_fn(|| accumulator.take_ready()).collect();
        Box::pin(futures::stream::iter(events))
    }
}

struct Execute(Rc<RefCell<Vec<ToolCall>>>);

struct SharedModel(Rc<Model>);
impl LlmPort for SharedModel {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
        self.0.capabilities(model)
    }
    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        self.0.estimate_request(request)
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.0.stream_chat(request)
    }
}
impl ToolExecutor for Execute {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        self.0.borrow_mut().push(call.clone());
        Box::pin(async move { ToolResult::success(call.id, json!({"fictional":true})) })
    }
}

fn items() -> Vec<Value> {
    vec![
        json!({"id":"r","type":"reasoning","summary":[],"encrypted_content":"opaque+fixture="}),
        json!({"id":"i-1","type":"function_call","call_id":"call-1","name":"read",
            "arguments":"{\"path\":\"first\"}"}),
        json!({"id":"i-2","type":"function_call","call_id":"call-2","name":"read",
            "arguments":"{\"path\":\"second\"}"}),
    ]
}

fn frames(items: &[Value]) -> Vec<Value> {
    let mut frames = vec![json!({"type":"response.created","response":{
        "id":"response","status":"in_progress"}})];
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
        }
        if item["type"] == "message" {
            added["content"] = json!([]);
        }
        if item["type"] == "reasoning" {
            added["encrypted_content"] = Value::Null;
        }
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "function_call" {
            frames.push(
                json!({"type":"response.function_call_arguments.delta","output_index":index,
                "item_id":item["id"],"delta":item["arguments"]}),
            );
        }
        if item["type"] == "message" {
            frames.push(
                json!({"type":"response.output_text.delta","output_index":index,
                "content_index":0,"item_id":item["id"],"delta":item["content"][0]["text"]}),
            );
        }
        frames.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    frames.push(json!({"type":"response.completed","response":{
        "id":"response","status":"completed","output":items}}));
    frames
}

fn answer() -> Vec<Value> {
    frames(
        &[json!({"id":"answer","type":"message","status":"completed",
        "role":"assistant","phase":"final_answer","content":[{
            "type":"output_text","text":"Fictional answer.","annotations":[]}]})],
    )
}

fn runner(model: Rc<Model>, executed: &Rc<RefCell<Vec<ToolCall>>>) -> AgentRunner {
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolDefinition::new(
                ToolSchema {
                    name: ToolName::new("read").unwrap(),
                    description: "Fictional caller-owned tool".into(),
                    parameters: json!({"type":"object","properties":{"path":{"type":"string"},
            "offset":{"type":"integer"}},"required":["path"]}),
                },
                Execute(Rc::clone(executed)),
            )
            .with_access(nanus_domain::ToolAccess::Read),
        )
        .unwrap();
    let admitted = model.adapter.is_some();
    // This LlmPort remains caller-owned; no stock composition or secret store is invoked.
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(SharedModel(model)));
    let runner = AgentRunner::new(
        llm,
        ToolRegistryHandle::new(registry),
        "Fictional video instructions",
        AgentConfig::new(4, 2, "gpt-6-astra", 16384).unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap();
    if admitted {
        runner.with_request_budget(8192, 0)
    } else {
        runner
    }
}

#[tokio::test]
async fn real_minimal_runner_retains_original_items_sibling_outputs_and_phase_through_reload() {
    verify_runner_replay(None).await;
}

#[tokio::test]
async fn real_admitted_requests_survive_runner_tool_results_and_v2_reload() {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    config.set_response_limits(ResponseLimits::new(8192, 8192, 65536, 100, 256, 1024).unwrap());
    config.set_function_strictness(Some(false));
    verify_runner_replay(Some(OpenAiLlm::new(config).unwrap())).await;
}

fn assert_first_batch(model: &Model, executed: &RefCell<Vec<ToolCall>>, items: &[Value]) {
    assert_eq!(
        executed
            .borrow()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-1", "call-2"]
    );
    let messages = model.requests.borrow()[1].messages.clone();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Assistant {
        replay:Some(replay), .. } if replay.blocks.as_slice()==items))
    );
    let outputs: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, vec!["call-1", "call-2"]);
}

async fn verify_runner_replay(adapter: Option<OpenAiLlm>) {
    let items = items();
    let model = Rc::new(Model {
        scripts: RefCell::new(vec![frames(&items), answer(), answer()]),
        requests: RefCell::new(Vec::new()),
        adapter,
        bodies: RefCell::new(Vec::new()),
    });
    let executed = Rc::new(RefCell::new(Vec::new()));
    let runner = runner(Rc::clone(&model), &executed);
    let mut session = Session::new(SessionId::new("fictional-replay"), 123, "/fictional");
    assert!(
        runner
            .run_turn(&mut session, "Inspect both.", &mut Silent, None)
            .await
            .unwrap()
            .is_success()
    );
    assert_first_batch(&model, &executed, &items);
    let raw = session.try_to_jsonl().unwrap();
    let mut loaded = Session::from_jsonl(&raw).unwrap();
    assert_eq!(loaded, session);
    assert!(
        loaded
            .derive_messages()
            .iter()
            .any(|message| matches!(message,
        Message::Assistant { replay:Some(replay), .. }
        if replay.blocks[0]["phase"]=="final_answer"))
    );
    assert!(
        runner
            .run_turn(&mut loaded, "Continue.", &mut Silent, None)
            .await
            .unwrap()
            .is_success()
    );
    if model.adapter.is_some() {
        let bodies = model.bodies.borrow();
        assert_eq!(&bodies[1]["input"].as_array().unwrap()[1..4], items);
        assert_eq!(bodies[2]["input"][6]["phase"], "final_answer");
        assert!(
            model
                .requests
                .borrow()
                .iter()
                .all(|request| request.source_history.is_some())
        );
    }
    assert_eq!(executed.borrow().len(), 2);
    assert_eq!(
        Session::from_jsonl(&loaded.try_to_jsonl().unwrap()).unwrap(),
        loaded
    );
}

#[tokio::test]
async fn incomplete_or_crossed_decoder_replay_never_reaches_a_runner_tool_executor() {
    let valid = frames(&items());
    let mut incomplete = valid.clone();
    incomplete.pop();
    let mut crossed = valid;
    crossed[2]["item"]["encrypted_content"] = json!("");
    for frames in [incomplete, crossed] {
        let model = Rc::new(Model {
            scripts: RefCell::new(vec![frames]),
            requests: RefCell::new(Vec::new()),
            adapter: None,
            bodies: RefCell::new(Vec::new()),
        });
        let executed = Rc::new(RefCell::new(Vec::new()));
        let runner = runner(model, &executed);
        let mut session = Session::new(SessionId::new("fictional-failed"), 123, "/fictional");
        assert!(
            runner
                .run_turn(&mut session, "Inspect.", &mut Silent, None)
                .await
                .is_err()
        );
        assert!(executed.borrow().is_empty());
        assert!(!session.derive_messages().iter().any(|message| matches!(
            message,
            Message::Assistant {
                replay: Some(_),
                ..
            }
        )));
        assert_eq!(
            Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap(),
            session
        );
    }
}
