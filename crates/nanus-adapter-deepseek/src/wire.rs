//! The `DeepSeek` wire protocol: request encoding and server-sent-events decoding.
//!
//! This module is the only place that knows `DeepSeek`'s JSON shapes. Keeping it
//! separate means the precise — and occasionally surprising — rules can be read in
//! one sitting and tested without a network.

use nanus_domain::{Message, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, ReasoningEffort};
use serde_json::{Map, Value, json};

use crate::config::DeepSeekConfig;

/// The SSE field prefix carrying a payload.
const DATA_PREFIX: &str = "data:";

/// The sentinel `DeepSeek` sends after the last frame.
const DONE_SENTINEL: &str = "[DONE]";

/// The most tool calls one response may carry.
///
/// A ceiling on a number a provider supplies, because it sizes an allocation. No model calls
/// anything like this many tools in one response — the toolset has seven tools and the
/// harness runs four at once — so a response claiming more is malformed rather than ambitious.
const MAX_TOOL_CALLS: usize = 256;

/// Builds the JSON body for a chat completion.
///
/// Only the fields `DeepSeek` actually honours are sent. `presence_penalty`,
/// `frequency_penalty`, `logprobs`, `response_format`, `tool_choice`, `seed`, and
/// `user_id` are deliberately absent: the first two are documented as having no
/// effect, and `tool_choice` is rejected outright in thinking mode.
#[must_use]
pub fn build_request(config: &DeepSeekConfig, request: &ChatRequest) -> Value {
    // Precondition: a request without messages is meaningless, so it is asserted
    // here rather than sent and rejected upstream.
    assert!(
        !request.messages.is_empty(),
        "a chat request carries at least one message"
    );
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(config.model()));
    body.insert("messages".to_owned(), encode_messages(&request.messages));
    body.insert("stream".to_owned(), json!(true));
    // Usage rides on the final content chunk; asking for it explicitly is what
    // makes `include_usage` true, and DeepSeek tolerates the field only alongside
    // `stream: true`, which is always set here.
    body.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );
    body.insert("max_tokens".to_owned(), json!(config.max_tokens()));

    let effort = request
        .reasoning_effort
        .unwrap_or_else(|| config.reasoning_effort());
    insert_effort(&mut body, effort);

    if let Some(temperature) = request.temperature.or_else(|| config.temperature()) {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if !request.tools.is_empty() {
        body.insert("tools".to_owned(), encode_tools(&request.tools));
    }
    let encoded = Value::Object(body);
    // Postcondition: the mandatory fields are present, so a silently empty request
    // cannot be produced by a later edit.
    assert!(encoded.get("model").is_some());
    assert!(encoded.get("messages").is_some());
    encoded
}

/// Writes the thinking-mode fields.
///
/// `reasoning_effort: none` is expressed as `thinking: {"type": "disabled"}` with
/// `reasoning_effort` omitted, which is how `DeepSeek` disables thinking; sending
/// both would be contradictory.
fn insert_effort(body: &mut Map<String, Value>, effort: ReasoningEffort) {
    let Some(wire) = crate::wire_effort(effort) else {
        // Disabling thinking is expressed as a mode, not as an effort of zero, and
        // sending both fields would be contradictory.
        body.insert("thinking".to_owned(), json!({ "type": "disabled" }));
        return;
    };
    body.insert("thinking".to_owned(), json!({ "type": "enabled" }));
    body.insert("reasoning_effort".to_owned(), json!(wire));
}

/// Encodes the conversation in `DeepSeek`'s message shape.
#[must_use]
pub fn encode_messages(messages: &[Message]) -> Value {
    let encoded: Vec<Value> = messages.iter().map(encode_message).collect();
    Value::Array(encoded)
}

/// Encodes one message.
fn encode_message(message: &Message) -> Value {
    match message {
        Message::System { text } => json!({ "role": "system", "content": text }),
        Message::User { text } => json!({ "role": "user", "content": text }),
        Message::Assistant {
            text,
            reasoning,
            tool_calls,
        } => {
            let mut object = Map::new();
            object.insert("role".to_owned(), json!("assistant"));
            // Negative space that matters: DeepSeek answers 400 for a
            // null-content assistant message with no tool calls, so an empty turn
            // must send the empty string rather than null.
            let content = text.clone().unwrap_or_default();
            assert!(
                !content.is_empty() || !tool_calls.is_empty(),
                "an assistant turn carries text or tool calls"
            );
            object.insert("content".to_owned(), json!(content));
            if let Some(reasoning) = reasoning {
                // Replayed because the API requires earlier turns' reasoning when
                // the request carries tools.
                object.insert("reasoning_content".to_owned(), json!(reasoning));
            }
            if !tool_calls.is_empty() {
                object.insert("tool_calls".to_owned(), encode_tool_calls(tool_calls));
            }
            Value::Object(object)
        }
        Message::Tool {
            call_id, content, ..
        } => json!({
            "role": "tool",
            "tool_call_id": call_id.as_str(),
            "content": content,
        }),
    }
}

/// Encodes assistant tool calls.
fn encode_tool_calls(tool_calls: &[nanus_domain::ToolCall]) -> Value {
    let encoded: Vec<Value> = tool_calls
        .iter()
        .map(|call| {
            json!({
                "id": call.id.as_str(),
                "type": "function",
                "function": {
                    "name": call.name.as_str(),
                    // The arguments travel as a JSON *string*, which is the shape
                    // the API both accepts and produces.
                    "arguments": call.arguments.to_string(),
                },
            })
        })
        .collect();
    Value::Array(encoded)
}

/// Encodes the tool catalogue.
///
/// Only `name`, `description`, and `parameters` are placed on the wire. This is
/// the allowlist the domain asserts: a tool's executor, timeout, and presenters
/// must never leak into a model request.
#[must_use]
pub fn encode_tools(tools: &[ToolSchema]) -> Value {
    let encoded: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name.as_str(),
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect();
    Value::Array(encoded)
}

/// A decoder for `DeepSeek`'s server-sent-events framing.
///
/// The decoder is a byte sink rather than a line reader because a network chunk can
/// split a frame anywhere, including mid-character. It also ignores keep-alive
/// comment lines and blank separators, which `DeepSeek` interleaves freely.
#[derive(Debug, Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    done: bool,
}

impl SseDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            done: false,
        }
    }

    /// Returns `true` once the terminal sentinel has been seen.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Feeds a network chunk and returns every complete payload it completed.
    ///
    /// A payload is the text after `data:` on a line, with the trailing newline
    /// removed. The `[DONE]` sentinel is consumed rather than returned.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut payloads = Vec::new();
        // Only whole lines are consumed, so a partial frame stays buffered.
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=index).collect();
            let Some(payload) = decode_line(&line) else {
                continue;
            };
            if payload == DONE_SENTINEL {
                self.done = true;
                continue;
            }
            payloads.push(payload);
        }
        // Postcondition: whatever remains is a partial line, never a whole one, so a
        // frame cannot be emitted twice.
        assert!(!self.pending.contains(&b'\n'), "whole lines are drained");
        payloads
    }

    /// Decodes whatever remains after the stream ends.
    ///
    /// A server that closes without a final newline still sent a complete frame, so
    /// discarding the tail would lose the last delta of a response.
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        let payload = decode_line(&line)?;
        // The sentinel is tested here too, and not only in `push`: the last frame of a stream
        // frequently arrives without a trailing newline, so this is *the* path `[DONE]` takes
        // when a server closes immediately after it. Returning it made `observe_line` try to
        // parse `[DONE]` as JSON, which failed the whole turn — an answer that had fully
        // arrived was thrown away because the goodbye was not understood.
        if payload == DONE_SENTINEL {
            self.done = true;
            return None;
        }
        Some(payload)
    }
}

/// Returns a string field's value when it is present and non-empty.
///
/// A delta field is frequently present-but-empty — a chunk that continues a tool
/// call carries `"arguments": ""` — and an empty value carries no information, so
/// treating it as absent keeps the callers free of the same check.
fn non_empty_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Decodes one line, returning its payload when it carries one.
fn decode_line(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return None;
    }
    // Keep-alive comments start with a colon and carry no payload.
    if trimmed.starts_with(':') {
        return None;
    }
    let payload = trimmed.strip_prefix(DATA_PREFIX)?.trim_start();
    Some(payload.to_owned())
}

/// One in-flight tool call being assembled from argument deltas.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates decoded frames into the events the agent loop consumes.
///
/// Tool-call arguments arrive as fragments, so a call cannot be reported until the
/// stream ends. The accumulator therefore holds one ready-queue plus the partial
/// calls, and emits the calls at [`StreamAccumulator::close`].
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    ready: Vec<LlmEvent>,
    calls: Vec<PartialToolCall>,
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
    ///
    /// A payload that is not valid JSON is reported as an error event rather than
    /// ignored: silence would hide a protocol change.
    pub fn observe_line(&mut self, payload: &str) {
        let parsed: Result<Value, _> = serde_json::from_str(payload);
        let Ok(value) = parsed else {
            self.ready.push(LlmEvent::Error(format!(
                "the server sent a frame that is not JSON: {}",
                truncate_for_message(payload)
            )));
            return;
        };
        self.observe_frame(&value);
    }

    /// Observes one decoded frame.
    pub fn observe_frame(&mut self, frame: &Value) {
        // Usage is null on every chunk but the last, so it is read whenever present.
        if let Some(usage) = frame.get("usage").filter(|value| !value.is_null()) {
            self.usage = Some(decode_usage(usage));
        }
        let Some(choices) = frame.get("choices").and_then(Value::as_array) else {
            return;
        };
        let Some(choice) = choices.first() else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(decode_finish_reason(reason));
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(reasoning) = non_empty_str(delta, "reasoning_content") {
            self.ready
                .push(LlmEvent::ReasoningDelta(reasoning.to_owned()));
        }
        if let Some(text) = non_empty_str(delta, "content") {
            self.ready.push(LlmEvent::TextDelta(text.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.observe_tool_call_delta(call);
            }
        }
    }

    /// Folds one `tool_calls` delta into the partial call it belongs to.
    fn observe_tool_call_delta(&mut self, delta: &Value) {
        let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0);
        // An index beyond any plausible response is a protocol violation rather than a call:
        // it is used to size a vector, so an index of 2^64-1 would allocate until the process
        // died. Folding it into slot zero keeps the arguments in order rather than dropping
        // them, which is what the previous line already did for an index that would not fit a
        // `usize` — a case that cannot arise on a 64-bit target, making that guard alone
        // insufficient.
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index < MAX_TOOL_CALLS)
            .unwrap_or_default();
        while self.calls.len() <= index {
            self.calls.push(PartialToolCall::default());
        }
        let Some(slot) = self.calls.get_mut(index) else {
            return;
        };
        // The id arrives on the first delta for a call; later deltas omit it or
        // repeat it, so a non-empty value is authoritative either way.
        if let Some(id) = non_empty_str(delta, "id") {
            id.clone_into(&mut slot.id);
        }
        let Some(function) = delta.get("function") else {
            return;
        };
        if let Some(name) = non_empty_str(function, "name") {
            name.clone_into(&mut slot.name);
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            slot.arguments.push_str(arguments);
        }
    }

    /// Emits the accumulated tool calls and the terminal event.
    ///
    /// Called when the stream ends, whether by the `[DONE]` sentinel or by the
    /// socket closing.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let calls = std::mem::take(&mut self.calls);
        for (index, call) in calls.into_iter().enumerate() {
            if call.name.is_empty() {
                // A call with no name carries no information; reporting it would
                // make the agent loop dispatch a tool that does not exist.
                continue;
            }
            // A name the domain would reject can never be dispatched, so it is
            // reported rather than quietly substituted: a hallucinated tool name is
            // worth surfacing, and inventing a valid one would dispatch the wrong
            // tool.
            let Ok(name) = nanus_domain::ToolName::new(call.name.clone()) else {
                self.ready.push(LlmEvent::Error(format!(
                    "the model requested a tool whose name is not usable: {:?}",
                    truncate_for_message(&call.name)
                )));
                continue;
            };
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            self.ready.push(LlmEvent::ToolCallDelta {
                index,
                id: Some(nanus_domain::ToolCallId::new(call.id)),
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

    /// Returns `true` once the terminal event has been emitted.
    ///
    /// A caller uses this to tell "nothing more is coming" from "nothing has arrived
    /// yet", which is the difference between a finished response and a stalled one.
    /// The decoder loop does not need it — it stops reading as soon as the terminal
    /// event is queued — so it exists for embedders and for the tests that pin the
    /// close-once behaviour.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
}

/// Decodes a `usage` object.
fn decode_usage(value: &Value) -> Usage {
    let field = |name: &str| -> u32 {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|number| u32::try_from(number).ok())
            .unwrap_or_default()
    };
    // DeepSeek reports cache accounting under its own names; absence means zero
    // rather than an error, because not every response carries the breakdown.
    let details = field("prompt_cache_hit_tokens");
    let missed = field("prompt_cache_miss_tokens");
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .unwrap_or(0);
    Usage {
        prompt_tokens: field("prompt_tokens"),
        completion_tokens: field("completion_tokens"),
        reasoning_tokens: reasoning,
        cache_hit_tokens: details,
        cache_miss_tokens: missed,
    }
}

/// Decodes a `finish_reason` string.
fn decode_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Unknown(other.to_owned()),
    }
}

/// Shortens a frame for an error message.
fn truncate_for_message(payload: &str) -> String {
    const MAX: usize = 200;
    if payload.len() <= MAX {
        return payload.to_owned();
    }
    let mut end = MAX;
    while end > 0 && !payload.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    payload.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::{ToolCall, ToolCallId, ToolName};

    fn config() -> DeepSeekConfig {
        DeepSeekConfig::new(crate::MODEL_FLASH, "test-key")
    }

    /// Builds a tool name for a test fixture.
    ///
    /// A panic here is the assertion that the literal is a valid name, so clippy's
    /// production-code rule does not apply.
    #[allow(clippy::panic)]
    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest::new(crate::MODEL_FLASH, messages)
    }

    #[test]
    fn request_carries_streaming_and_usage_flags() {
        let body = build_request(&config(), &request(vec![Message::user("hi")]));
        // DeepSeek tolerates `stream_options` only alongside `stream: true`, so both
        // must always be present together.
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["model"], json!(crate::MODEL_FLASH));
    }

    #[test]
    fn dead_fields_are_never_sent() {
        let body = build_request(&config(), &request(vec![Message::user("hi")]));
        // These are either documented as having no effect or rejected in thinking
        // mode, so sending them would be misleading.
        for field in [
            "presence_penalty",
            "frequency_penalty",
            "logprobs",
            "response_format",
            "tool_choice",
            "seed",
            "user_id",
            "user",
        ] {
            assert!(body.get(field).is_none(), "{field} must not be sent");
        }
    }

    #[test]
    fn default_effort_enables_thinking() {
        let body = build_request(&config(), &request(vec![Message::user("hi")]));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn minimal_effort_disables_thinking_and_omits_the_effort_field() {
        let mut config = config();
        config.set_reasoning_effort(ReasoningEffort::Minimal);
        let body = build_request(&config, &request(vec![Message::user("hi")]));
        assert_eq!(body["thinking"]["type"], json!("disabled"));
        // Sending both would be contradictory.
        assert!(body.get("reasoning_effort").is_none());

        // Pair assertion: the non-minimal levels enable thinking and name an effort.
        config.set_reasoning_effort(ReasoningEffort::High);
        let body = build_request(&config, &request(vec![Message::user("hi")]));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("max"));
    }

    #[test]
    fn an_empty_assistant_turn_sends_an_empty_string_not_null() {
        let message = Message::Assistant {
            text: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call-1"),
                name: tool_name("read"),
                arguments: json!({ "file_path": "a.txt" }),
            }],
        };
        let body = build_request(&config(), &request(vec![message]));
        let encoded = &body["messages"][0];
        // This is the exact shape the live API requires; null produces a 400.
        assert_eq!(encoded["content"], json!(""));
        assert!(!encoded["content"].is_null());
    }

    #[test]
    fn tool_arguments_are_encoded_as_a_json_string() {
        let message = Message::Assistant {
            text: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call-7"),
                name: tool_name("shell"),
                arguments: json!({ "command": "ls" }),
            }],
        };
        let body = build_request(&config(), &request(vec![message]));
        let arguments = &body["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert!(arguments.is_string(), "arguments travel as a string");
        let parsed: Value =
            serde_json::from_str(arguments.as_str().unwrap_or("null")).unwrap_or_default();
        assert_eq!(parsed["command"], json!("ls"));
    }

    #[test]
    fn reasoning_content_is_replayed_for_earlier_turns() {
        let message = Message::Assistant {
            text: Some("answer".to_owned()),
            reasoning: Some("because".to_owned()),
            tool_calls: Vec::new(),
        };
        let body = build_request(&config(), &request(vec![message]));
        // Required by the API when the request carries tools; harmless otherwise.
        assert_eq!(body["messages"][0]["reasoning_content"], json!("because"));
    }

    #[test]
    fn only_the_three_schema_fields_reach_the_wire() {
        let schema = ToolSchema {
            name: tool_name("read"),
            description: "Read a file".to_owned(),
            parameters: json!({ "type": "object" }),
        };
        let tools = encode_tools(&[schema]);
        let function = &tools[0]["function"];
        let Some(object) = function.as_object() else {
            return;
        };
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["description", "name", "parameters"]);
    }

    #[test]
    fn the_decoder_reassembles_frames_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        // A frame split mid-payload, which is the case a line reader gets wrong.
        let first = decoder.push(b"data: {\"a\":");
        assert!(first.is_empty(), "a partial frame yields nothing");
        let second = decoder.push(b"1}\n\n");
        assert_eq!(second, vec!["{\"a\":1}".to_owned()]);
    }

    #[test]
    fn the_decoder_ignores_comments_and_blank_lines() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(b": keep-alive\n\ndata: {\"x\":1}\n");
        assert_eq!(payloads, vec!["{\"x\":1}".to_owned()]);
        assert!(!decoder.is_done());
    }

    #[test]
    fn the_decoder_stops_at_the_sentinel() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(b"data: [DONE]\n");
        assert!(payloads.is_empty());
        // The sentinel is consumed, not surfaced as a payload.
        assert!(decoder.is_done());
    }

    #[test]
    fn the_decoder_returns_a_final_frame_without_a_newline() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"z\":9}").is_empty());
        // A server that closes without a trailing newline still sent a frame.
        assert_eq!(decoder.finish(), Some("{\"z\":9}".to_owned()));
        assert_eq!(decoder.finish(), None, "the tail is consumed once");
    }

    #[test]
    fn a_stream_accumulates_deltas_and_emits_tool_calls_at_the_end() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#);
        accumulator.observe_line(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#);
        accumulator.observe_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"file"}}]}}]}"#,
        );
        accumulator.observe_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"_path\":\"a\"}"}}]}}]}"#,
        );
        accumulator.observe_line(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#);

        let mut events = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            events.push(event);
        }
        // Before the stream ends, only the text and reasoning deltas are known.
        assert!(matches!(events.first(), Some(LlmEvent::ReasoningDelta(text)) if text == "think "));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::TextDelta(t) if t == "Hello"))
        );

        accumulator.close();
        let mut tail = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            tail.push(event);
        }
        let call = tail.iter().find_map(|event| match event {
            LlmEvent::ToolCallDelta {
                name,
                arguments_delta,
                ..
            } => Some((name.clone(), arguments_delta.clone())),
            _ => None,
        });
        let Some((name, arguments)) = call else {
            panic!("a tool call is emitted once the stream ends");
        };
        assert_eq!(
            name.map(|name| name.as_str().to_owned()),
            Some("read".to_owned())
        );
        // The fragments are joined verbatim; assembling them into a JSON value is
        // the ports crate's `ToolCallAssembler`, so the adapter does not parse.
        assert_eq!(arguments, r#"{"file_path":"a"}"#);
        assert!(tail.iter().any(|event| matches!(event, LlmEvent::Usage(_))));
        assert!(
            tail.iter()
                .any(|event| matches!(event, LlmEvent::Finished { .. }))
        );
    }

    #[test]
    fn malformed_tool_arguments_are_passed_through_rather_than_lost() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{not json"}}]}}]}"#,
        );
        accumulator.close();
        let mut found = None;
        while let Some(event) = accumulator.take_ready() {
            if let LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } = event
            {
                found = Some(arguments_delta);
            }
        }
        // The fragments survive verbatim, so the layer that parses them can report a
        // validation failure instead of the harness dropping the call.
        assert_eq!(found.as_deref(), Some("{not json"));
    }

    #[test]
    fn a_tool_call_with_no_arguments_yields_an_empty_fragment() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"now"}}]}}]}"#,
        );
        accumulator.close();
        let mut found = None;
        while let Some(event) = accumulator.take_ready() {
            if let LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } = event
            {
                found = Some(arguments_delta);
            }
        }
        // A tool with no parameters sends nothing at all, which is the empty string
        // rather than a missing field.
        assert_eq!(found.as_deref(), Some(""));
    }

    #[test]
    fn a_non_json_frame_becomes_an_error_event() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line("this is not json");
        // Silence would hide a protocol change, so the frame is reported.
        let event = accumulator.take_ready();
        assert!(matches!(event, Some(LlmEvent::Error(_))));
    }

    #[test]
    fn finish_reasons_map_to_the_domain_vocabulary() {
        assert_eq!(decode_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(decode_finish_reason("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(decode_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            decode_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
        // An unrecognised reason is preserved rather than mapped to `Stop`, which
        // would make an unknown state look like a clean finish.
        assert_eq!(
            decode_finish_reason("something_new"),
            FinishReason::Unknown("something_new".to_owned())
        );
    }

    #[test]
    fn usage_decoding_tolerates_a_missing_breakdown() {
        let usage = decode_usage(&json!({ "prompt_tokens": 3, "completion_tokens": 1 }));
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 1);
        // Absent cache accounting means zero, not an error.
        assert_eq!(usage.cache_hit_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
        assert_eq!(usage.total_tokens(), 4);
    }

    #[test]
    fn usage_decoding_reads_the_reasoning_breakdown() {
        let usage = decode_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            "completion_tokens_details": { "reasoning_tokens": 30 }
        }));
        assert_eq!(usage.reasoning_tokens, 30);
        assert_eq!(usage.cache_hit_tokens, 80);
        assert_eq!(usage.cache_miss_tokens, 20);
        assert_eq!(usage.total_tokens(), 150);
    }

    #[test]
    fn a_tool_call_with_no_name_is_dropped_rather_than_dispatched() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"arguments":"{}"}}]}}]}"#,
        );
        accumulator.close();
        let mut saw_call = false;
        while let Some(event) = accumulator.take_ready() {
            if matches!(event, LlmEvent::ToolCallDelta { .. }) {
                saw_call = true;
            }
        }
        // Dispatching a nameless call would ask the registry for a tool that cannot
        // exist.
        assert!(!saw_call);
    }

    #[test]
    fn closing_twice_emits_one_terminal_event() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.close();
        let first = accumulator.take_ready();
        assert!(matches!(first, Some(LlmEvent::Finished { .. })));
        accumulator.close();
        // The second close is a no-op, so a finish is never reported twice.
        assert!(accumulator.take_ready().is_none());
        assert!(accumulator.is_closed());
    }
    /// A sentinel that arrives without a trailing newline still ends the stream cleanly.
    ///
    /// `push` handles `[DONE]` for whole lines; the *tail* is what `finish` decodes, and a
    /// server that closes immediately after `data: [DONE]` leaves exactly that. Returning the
    /// sentinel as a payload made the caller try to parse `[DONE]` as JSON — an `LlmEvent::Error`
    /// that failed a turn whose answer had already arrived in full.
    #[test]
    fn a_sentinel_with_no_trailing_newline_ends_the_stream() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(b"data: {\"id\":1}\ndata: [DONE]");
        assert_eq!(
            payloads.len(),
            1,
            "the whole frame is returned: {payloads:?}"
        );
        assert!(
            decoder.finish().is_none(),
            "the sentinel is consumed rather than returned as a frame"
        );
        assert!(decoder.done, "and the stream is marked finished");
    }

    /// The other half: a genuine tail — a frame with no newline — is still returned, because
    /// dropping it would lose the last delta of a response.
    #[test]
    fn a_real_tail_is_still_decoded() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"id\":1}").is_empty());
        assert_eq!(
            decoder.finish().as_deref(),
            Some("{\"id\":1}"),
            "a frame that never got its newline is still a frame"
        );
    }
}
