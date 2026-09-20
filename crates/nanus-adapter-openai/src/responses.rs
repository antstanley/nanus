//! The Responses API: the wire the `ChatGPT` subscription speaks.
//!
//! A subscription-backed request is not a chat completion. The conversation goes as a flat list of
//! *items* — a user turn, the assistant's output, the function calls it made, and their results —
//! with the system prompt lifted into a top-level `instructions` field, and the reasoning control
//! moved under a `reasoning` object. Streaming is a sequence of named events rather than a stream
//! of `choices` deltas, so the decoder dispatches on each payload's own `type`.
//!
//! Only what the harness uses is encoded: the seven tools, the reasoning step, the output ceiling,
//! and the conversation. Sampling fields nobody reads, and the several Responses-only features the
//! harness has no place for, are absent for the same reason the chat path leaves them absent.

use nanus_domain::{Message, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, ReasoningEffort};
use serde_json::{Map, Value, json};

use crate::config::OpenAiConfig;

/// The most tool calls one response may carry.
const MAX_TOOL_CALLS: usize = 256;

/// Builds the JSON body for a Responses request.
#[must_use]
pub fn build_request(config: &OpenAiConfig, request: &ChatRequest) -> Value {
    assert!(
        !request.messages.is_empty(),
        "a request carries at least one message"
    );
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(config.model()));
    body.insert("stream".to_owned(), json!(true));
    // Nothing is kept server-side: the session log is the record, and a stored response would be a
    // second one that outlives the conversation it belongs to.
    body.insert("store".to_owned(), json!(false));
    if let Some(instructions) = instructions(&request.messages) {
        body.insert("instructions".to_owned(), json!(instructions));
    }
    body.insert("input".to_owned(), encode_input(&request.messages));
    // The provider's ceiling, not the configured budget: a request above it is refused rather than
    // truncated, so sending it would fail every step.
    body.insert(
        "max_output_tokens".to_owned(),
        json!(config.effective_max_tokens()),
    );
    let effort = request
        .reasoning_effort
        .unwrap_or_else(|| config.reasoning_effort());
    body.insert(
        "reasoning".to_owned(),
        json!({ "effort": crate::effort_spelling(effort) }),
    );
    if let Some(temperature) = request.temperature.or_else(|| config.temperature()) {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if !request.tools.is_empty() {
        body.insert("tools".to_owned(), encode_tools(&request.tools));
    }
    let encoded = Value::Object(body);
    assert!(encoded.get("model").is_some());
    assert!(encoded.get("input").is_some());
    encoded
}

/// Joins the system turns into the single `instructions` string the API expects.
///
/// The API has one instruction field rather than a system role, so several system turns are joined
/// rather than dropped; `None` when there are none, so an absent prompt is absent rather than empty.
fn instructions(messages: &[Message]) -> Option<String> {
    let mut joined = String::new();
    for message in messages {
        if let Message::System { text } = message {
            if !joined.is_empty() {
                joined.push_str("\n\n");
            }
            joined.push_str(text);
        }
    }
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Encodes the conversation as the flat list of input items.
///
/// A user or assistant turn is one item; a tool call and its result are items of their own, because
/// that is the shape the API replays them in. They stay in order, so a call is emitted before the
/// result that answers it.
#[must_use]
pub fn encode_input(messages: &[Message]) -> Value {
    let mut items: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            // Lifted into `instructions`; not repeated here.
            Message::System { .. } => {}
            Message::User { text } => items.push(json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": text }],
            })),
            Message::Assistant {
                text, tool_calls, ..
            } => {
                if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                    items.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id.as_str(),
                        "name": call.name.as_str(),
                        "arguments": call.arguments.to_string(),
                    }));
                }
            }
            Message::Tool {
                call_id, content, ..
            } => items.push(json!({
                "type": "function_call_output",
                "call_id": call_id.as_str(),
                "output": content,
            })),
        }
    }
    Value::Array(items)
}

/// Encodes the tool catalogue.
///
/// Only `name`, `description`, and `parameters` are placed on the wire — the same allowlist the
/// chat path keeps, in the flat shape this API uses.
#[must_use]
pub fn encode_tools(tools: &[ToolSchema]) -> Value {
    let encoded: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name.as_str(),
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect();
    Value::Array(encoded)
}

/// One in-flight tool call being assembled from argument deltas.
#[derive(Debug, Default)]
struct PartialCall {
    /// The item id the deltas are keyed by, which is not the call id.
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Accumulates Responses events into the events the agent loop consumes.
///
/// The same shape as the chat accumulator, for the same reason: a function call's arguments arrive
/// as fragments keyed by an item, so the call cannot be reported until the stream ends.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    ready: Vec<LlmEvent>,
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    closed: bool,
}

impl StreamAccumulator {
    /// Takes the next ready event, if any.
    pub fn take_ready(&mut self) -> Option<LlmEvent> {
        if self.ready.is_empty() {
            return None;
        }
        Some(self.ready.remove(0))
    }

    /// Records a failure as a terminal event.
    pub fn fail(&mut self, message: String) {
        self.ready.push(LlmEvent::Error(message));
        self.closed = true;
    }

    /// Observes one decoded SSE payload.
    pub fn observe_line(&mut self, payload: &str) {
        let parsed: Result<Value, _> = serde_json::from_str(payload);
        let Ok(value) = parsed else {
            self.ready.push(LlmEvent::Error(format!(
                "the server sent a frame that is not JSON: {}",
                crate::wire::truncate_for_message(payload)
            )));
            return;
        };
        self.observe_frame(&value);
    }

    /// Observes one decoded event.
    pub fn observe_frame(&mut self, frame: &Value) {
        // A failure can arrive as an `error` event or as a `response.failed` one, and either way it
        // is the reason the turn stopped.
        if let Some(message) = error_message(frame) {
            self.fail(message);
            return;
        }
        let Some(kind) = frame.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "response.output_text.delta" => {
                if let Some(text) = non_empty_str(frame, "delta") {
                    self.ready.push(LlmEvent::TextDelta(text.to_owned()));
                }
            }
            // A model that summarizes its reasoning streams it under one of these names.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = non_empty_str(frame, "delta") {
                    self.ready.push(LlmEvent::ReasoningDelta(text.to_owned()));
                }
            }
            "response.output_item.added" => self.observe_item_added(frame.get("item")),
            "response.function_call_arguments.delta" => {
                if let (Some(id), Some(delta)) = (
                    non_empty_str(frame, "item_id"),
                    frame.get("delta").and_then(Value::as_str),
                ) && let Some(call) = self.calls.iter_mut().find(|call| call.item_id == id)
                {
                    call.arguments.push_str(delta);
                }
            }
            "response.output_item.done" => self.observe_item_done(frame.get("item")),
            "response.completed" => {
                if let Some(usage) = frame
                    .get("response")
                    .and_then(|response| response.get("usage"))
                    .filter(|usage| !usage.is_null())
                {
                    self.usage = Some(decode_usage(usage));
                }
                self.finish = Some(FinishReason::Stop);
            }
            // The response ran out of room rather than finishing, which is a truncated answer and
            // not a failure.
            "response.incomplete" => self.finish = Some(FinishReason::Length),
            _ => {}
        }
    }

    /// Starts a partial call when a function call item is added.
    fn observe_item_added(&mut self, item: Option<&Value>) {
        let Some(item) = item else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        if self.calls.len() >= MAX_TOOL_CALLS {
            return;
        }
        self.calls.push(PartialCall {
            item_id: non_empty_str(item, "id").unwrap_or_default().to_owned(),
            call_id: non_empty_str(item, "call_id")
                .unwrap_or_default()
                .to_owned(),
            name: non_empty_str(item, "name").unwrap_or_default().to_owned(),
            arguments: item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }

    /// Completes a partial call when its item is done.
    ///
    /// The done item carries the whole arguments string, so it replaces whatever the deltas built
    /// rather than being appended to it.
    fn observe_item_done(&mut self, item: Option<&Value>) {
        let Some(item) = item else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let id = non_empty_str(item, "id").unwrap_or_default();
        let Some(call) = self.calls.iter_mut().find(|call| call.item_id == id) else {
            return;
        };
        if let Some(call_id) = non_empty_str(item, "call_id") {
            call_id.clone_into(&mut call.call_id);
        }
        if let Some(name) = non_empty_str(item, "name") {
            name.clone_into(&mut call.name);
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            arguments.clone_into(&mut call.arguments);
        }
    }

    /// Emits the accumulated tool calls and the terminal event.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        for (index, call) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            if call.name.is_empty() {
                continue;
            }
            let Ok(name) = nanus_domain::ToolName::new(call.name.clone()) else {
                self.ready.push(LlmEvent::Error(format!(
                    "the model requested a tool whose name is not usable: {:?}",
                    crate::wire::truncate_for_message(&call.name)
                )));
                continue;
            };
            self.ready.push(LlmEvent::ToolCallDelta {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                id: Some(nanus_domain::ToolCallId::new(call.call_id)),
                name: Some(name),
                arguments_delta: call.arguments,
            });
        }
        if let Some(usage) = self.usage.take() {
            self.ready.push(LlmEvent::Usage(usage));
        }
        let reason = self.finish.take().unwrap_or(FinishReason::Stop);
        self.ready.push(LlmEvent::Finished { reason });
    }
}

/// Returns the message of a failure event, when the frame is one.
fn error_message(frame: &Value) -> Option<String> {
    match frame.get("type").and_then(Value::as_str) {
        Some("error") => Some(format!(
            "the provider reported an error: {}",
            message_of(frame)
        )),
        Some("response.failed") => Some(format!(
            "the response failed: {}",
            frame
                .get("response")
                .map_or_else(|| message_of(frame), message_of)
        )),
        _ => None,
    }
}

/// Reads a message out of an object, falling back to its whole rendering.
fn message_of(value: &Value) -> String {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Returns a string field's value when it is present and non-empty.
fn non_empty_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Decodes a Responses `usage` object.
fn decode_usage(value: &Value) -> Usage {
    let number = |name: &str| -> u32 {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_default()
    };
    let nested = |parent: &str, name: &str| -> u32 {
        value
            .get(parent)
            .and_then(|object| object.get(name))
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_default()
    };
    let prompt_tokens = number("input_tokens");
    let cache_hit_tokens = nested("input_tokens_details", "cached_tokens");
    // The two counters partition the prompt, so an unreported miss is derived rather than left at
    // zero — the same rule the chat path follows.
    Usage {
        prompt_tokens,
        completion_tokens: number("output_tokens"),
        reasoning_tokens: nested("output_tokens_details", "reasoning_tokens"),
        cache_hit_tokens,
        cache_miss_tokens: prompt_tokens.saturating_sub(cache_hit_tokens),
    }
}

/// The reasoning step's wire name, exposed for tests.
#[must_use]
pub fn effort_name(effort: ReasoningEffort) -> &'static str {
    crate::effort_spelling(effort)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Vendor;
    use nanus_domain::{ToolCall, ToolCallId, ToolName};

    fn config() -> OpenAiConfig {
        OpenAiConfig::new(Vendor::OpenAi, "gpt-5.3-codex", "token")
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest::new("gpt-5.3-codex", messages)
    }

    fn name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    /// Observes events through a fresh accumulator and returns what it emitted.
    fn observe(events: &[Value]) -> Vec<LlmEvent> {
        let mut accumulator = StreamAccumulator::default();
        for event in events {
            accumulator.observe_frame(event);
        }
        accumulator.close();
        let mut out = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            out.push(event);
        }
        out
    }

    /// The prompt is lifted into `instructions` and the turns become flat input items.
    #[test]
    fn the_request_lifts_the_prompt_and_flattens_the_turns() {
        let body = build_request(
            &config(),
            &request(vec![
                Message::system("be terse"),
                Message::system("and kind"),
                Message::user("hello"),
            ]),
        );
        assert_eq!(body["instructions"], json!("be terse\n\nand kind"));
        assert_eq!(body["input"][0]["role"], json!("user"));
        assert_eq!(body["input"][0]["content"][0]["type"], json!("input_text"));
        assert!(body.get("messages").is_none(), "the chat shape is not sent");
        assert_eq!(body["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["store"], json!(false));
        assert!(body["max_output_tokens"].as_u64().is_some());
        assert_eq!(body["reasoning"]["effort"], json!("medium"));
    }

    /// A system-only conversation still produces a body with the prompt and no turns.
    #[test]
    fn a_promptless_conversation_omits_instructions() {
        let body = build_request(&config(), &request(vec![Message::user("hi")]));
        assert!(body.get("instructions").is_none(), "no prompt is absent");
        assert_eq!(body["input"].as_array().map(Vec::len), Some(1));
    }

    /// A tool call and its result are items of their own, in order.
    #[test]
    fn a_tool_call_and_its_result_are_items_of_their_own() {
        let call = ToolCall {
            id: ToolCallId::new("call-1"),
            name: name("read"),
            arguments: json!({"file_path": "a.rs"}),
        };
        let body = build_request(
            &config(),
            &request(vec![
                Message::user("read it"),
                Message::Assistant {
                    text: None,
                    reasoning: None,
                    tool_calls: vec![call],
                },
                Message::Tool {
                    call_id: ToolCallId::new("call-1"),
                    content: String::from("contents"),
                    is_error: false,
                },
            ]),
        );
        let input = body["input"].as_array().cloned().unwrap_or_default();
        assert_eq!(input.len(), 3, "{body}");
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["call_id"], json!("call-1"));
        assert_eq!(input[1]["name"], json!("read"));
        assert_eq!(input[1]["arguments"], json!("{\"file_path\":\"a.rs\"}"));
        assert_eq!(input[2]["type"], json!("function_call_output"));
        assert_eq!(input[2]["output"], json!("contents"));
    }

    /// The tool catalogue is the flat shape this API uses, not the nested one.
    #[test]
    fn the_tool_catalogue_is_flat() {
        let tools = [ToolSchema {
            name: name("read"),
            description: String::from("reads a file"),
            parameters: json!({"type": "object"}),
        }];
        let encoded = encode_tools(&tools);
        assert_eq!(encoded[0]["type"], json!("function"));
        assert_eq!(encoded[0]["name"], json!("read"));
        assert_eq!(encoded[0]["description"], json!("reads a file"));
        assert!(
            encoded[0].get("function").is_none(),
            "the chat shape nests it; this one does not"
        );
    }

    /// Text and reasoning stream as they arrive, and the usage is decoded from the close.
    #[test]
    fn text_and_reasoning_stream_and_usage_is_decoded() {
        let events = observe(&[
            json!({"type":"response.reasoning_summary_text.delta","delta":"hmm"}),
            json!({"type":"response.output_text.delta","delta":"hi"}),
            json!({"type":"response.completed","response":{"usage":{
                "input_tokens":10,
                "output_tokens":2,
                "input_tokens_details":{"cached_tokens":4},
                "output_tokens_details":{"reasoning_tokens":1}
            }}}),
        ]);
        assert_eq!(events[0], LlmEvent::ReasoningDelta(String::from("hmm")));
        assert_eq!(events[1], LlmEvent::TextDelta(String::from("hi")));
        assert_eq!(
            events[2],
            LlmEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                reasoning_tokens: 1,
                cache_hit_tokens: 4,
                cache_miss_tokens: 6,
            })
        );
        assert_eq!(
            events[3],
            LlmEvent::Finished {
                reason: FinishReason::Stop
            }
        );
    }

    /// A function call is assembled from its argument deltas and reported when the stream ends.
    #[test]
    fn a_function_call_is_assembled_and_reported_at_the_end() {
        let events = observe(&[
            json!({"type":"response.output_item.added","item":{
                "type":"function_call","id":"item-1","call_id":"call-1","name":"read","arguments":""
            }}),
            json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"file"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"_path\":\"a\"}"}),
            json!({"type":"response.output_item.done","item":{
                "type":"function_call","id":"item-1","call_id":"call-1","name":"read",
                "arguments":"{\"file_path\":\"a\"}"
            }}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":3}}}),
        ]);
        let call = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ToolCallDelta { .. }))
            .expect("the call is reported");
        assert_eq!(
            *call,
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call-1")),
                name: Some(name("read")),
                arguments_delta: String::from("{\"file_path\":\"a\"}"),
            }
        );
    }

    /// A response that ran out of room is a truncated answer, not a failure.
    #[test]
    fn an_incomplete_response_is_a_truncated_answer() {
        let events = observe(&[json!({"type":"response.incomplete","response":{
            "incomplete_details":{"reason":"max_output_tokens"}
        }})]);
        assert_eq!(
            events[0],
            LlmEvent::Finished {
                reason: FinishReason::Length
            }
        );
    }

    /// A failure event is reported with its message rather than ending the stream silently.
    #[test]
    fn a_failed_response_is_an_error() {
        for frame in [
            json!({"type":"response.failed","response":{"error":{"message":"boom"}}}),
            json!({"type":"error","message":"boom"}),
        ] {
            let events = observe(&[frame]);
            assert!(
                matches!(&events[0], LlmEvent::Error(message) if message.contains("boom")),
                "{events:?}"
            );
        }
    }
}
