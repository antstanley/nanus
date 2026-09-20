//! The `OpenAI`-compatible wire protocol: request encoding and SSE decoding.
//!
//! This module is the only place that knows the JSON shapes shared by `OpenAI` and
//! z.ai. The two vendors differ in a handful of documented ways — which reasoning
//! control is sent, whether the credential is the same — and each difference is
//! read from [`Vendor`] here rather than being branched on a provider string
//! somewhere above.
//!
//! ## What is deliberately not sent
//!
//! - **An earlier turn's reasoning.** `DeepSeek` requires its `reasoning_content`
//!   back when a request carries tools; these vendors do not, and `OpenAI` rejects a
//!   request body containing a field it does not know. Reasoning is still written
//!   down in the transcript — it is what the interface draws and what a session
//!   reports — it is simply not part of the next request.
//! - **Dead sampling fields.** `presence_penalty`, `frequency_penalty`, `logprobs`,
//!   `response_format`, `tool_choice`, `seed`, and `user` are absent: the harness
//!   has no use for them, and a field nobody reads is a field nobody reviews.
//!
//! ## Why an empty assistant turn sends `content: ""`
//!
//! The same rule the domain enforces for `DeepSeek` holds here, and is asserted
//! rather than assumed: a turn that carries tool calls may carry no text, and a
//! `null` content in that position is refused by strict endpoints. The empty string
//! is understood by every compatible server.

use nanus_domain::{Message, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, ReasoningEffort};
use serde_json::{Map, Value, json};

use crate::config::{OpenAiConfig, Vendor};

/// The server-sent-events framer, shared with every adapter that streams this way.
pub use nanus_ports::SseFrames as SseDecoder;

/// The most tool calls one response may carry.
///
/// A ceiling on a number a provider supplies, because it sizes an allocation. No
/// model calls anything like this many tools in one response, so a response
/// claiming more is malformed rather than ambitious.
const MAX_TOOL_CALLS: usize = 256;

/// Builds the JSON body for a chat completion.
#[must_use]
pub fn build_request(config: &OpenAiConfig, request: &ChatRequest) -> Value {
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
    // Usage arrives only when it is asked for, and both vendors tolerate the field
    // alongside `stream: true`.
    body.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );
    // The provider's ceiling, not the configured budget: a request above it is
    // refused rather than truncated, so sending it would fail every step.
    // The field's *name* is the vendor's too: OpenAI's reasoning models reject
    // `max_tokens` and require `max_completion_tokens`.
    body.insert(
        config.vendor().output_token_field().to_owned(),
        json!(config.effective_max_tokens()),
    );

    let effort = request
        .reasoning_effort
        .unwrap_or_else(|| config.reasoning_effort());
    insert_effort(&mut body, config.vendor(), effort);

    if let Some(temperature) = request.temperature.or_else(|| config.temperature()) {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if !request.tools.is_empty() {
        body.insert("tools".to_owned(), encode_tools(&request.tools));
    }
    let encoded = Value::Object(body);
    // Postcondition: the mandatory fields are present, so a later edit cannot
    // produce a silently empty request.
    assert!(encoded.get("model").is_some());
    assert!(encoded.get("messages").is_some());
    encoded
}

/// Writes the reasoning control the vendor understands.
///
/// Both vendors take the same seven-word scale on `reasoning_effort`, so the neutral step travels
/// as written (see [`crate::effort_spelling`]). z.ai additionally needs the thinking switch named,
/// and it must be *enabled*: its GLM-5.3 models refuse a disabled switch, so turning thinking off
/// is asked for as the `none` effort that skips it rather than as the switch being turned off.
fn insert_effort(body: &mut Map<String, Value>, vendor: Vendor, effort: ReasoningEffort) {
    let spelling = crate::effort_spelling(effort);
    match vendor {
        Vendor::OpenAi => {
            body.insert("reasoning_effort".to_owned(), json!(spelling));
        }
        Vendor::Zai => {
            body.insert("thinking".to_owned(), json!({ "type": "enabled" }));
            body.insert("reasoning_effort".to_owned(), json!(spelling));
        }
    }
}

/// Encodes the conversation in the vendor's message shape.
#[must_use]
pub fn encode_messages(messages: &[Message]) -> Value {
    Value::Array(messages.iter().map(encode_message).collect())
}

/// Encodes one message.
fn encode_message(message: &Message) -> Value {
    match message {
        Message::System { text } => json!({ "role": "system", "content": text }),
        Message::User { text } => json!({ "role": "user", "content": text }),
        Message::Assistant {
            text, tool_calls, ..
        } => {
            let mut object = Map::new();
            object.insert("role".to_owned(), json!("assistant"));
            let content = text.clone().unwrap_or_default();
            // The domain drops an empty assistant turn before assembly, so a turn
            // that reached here carries text or tool calls; stating it as a
            // precondition is what keeps a later edit from sending neither.
            assert!(
                !content.is_empty() || !tool_calls.is_empty(),
                "an assistant turn carries text or tool calls"
            );
            object.insert("content".to_owned(), json!(content));
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
                    // The arguments travel as a JSON *string*, which is the shape the
                    // API both accepts and produces.
                    "arguments": call.arguments.to_string(),
                },
            })
        })
        .collect();
    Value::Array(encoded)
}

/// Encodes the tool catalogue.
///
/// Only `name`, `description`, and `parameters` are placed on the wire. This is the
/// allowlist the domain asserts: a tool's executor, timeout, and presenters must
/// never leak into a model request.
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
/// stream ends. The accumulator therefore holds one ready queue plus the partial
/// calls, and emits them at [`StreamAccumulator::close`].
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
        // A vendor may report a failure inside a 200 response, which is how a
        // quota problem arrives mid-stream. Reporting it as an error is the
        // difference between a turn that says why it stopped and one that ends
        // silently with nothing.
        if let Some(error) = frame.get("error").filter(|value| !value.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            self.fail(format!("the provider reported an error: {message}"));
            return;
        }
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
        self.observe_delta(delta);
    }

    /// Folds one `delta` object into the events and partial calls.
    fn observe_delta(&mut self, delta: &Value) {
        // z.ai reports a reasoning trace beside the visible text, under the name
        // DeepSeek uses; OpenAI's reasoning models do not surface theirs at all.
        if let Some(reasoning) =
            non_empty_str(delta, "reasoning_content").or_else(|| non_empty_str(delta, "reasoning"))
        {
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
        // An index beyond any plausible response is a protocol violation rather
        // than a call: it sizes a vector, so an index of 2^64-1 would allocate
        // until the process died. Folding it into slot zero keeps the arguments in
        // order rather than dropping them.
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

/// Decodes a `usage` object.
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
    let prompt_tokens = number("prompt_tokens");
    // OpenAI reports the cache hit inside `prompt_tokens_details`; a provider that
    // copied DeepSeek's shape reports it beside the prompt count. Both are read, and
    // the larger wins, so a response carrying either spelling is accounted for.
    let cache_hit_tokens =
        nested("prompt_tokens_details", "cached_tokens").max(number("prompt_cache_hit_tokens"));
    // A miss count is derived when it is not reported, rather than being left at
    // zero: the two counters partition the prompt, and a reader subtracting one
    // from the other would otherwise see the whole prompt as cached.
    let reported_miss = number("prompt_cache_miss_tokens");
    let cache_miss_tokens = if reported_miss > 0 {
        reported_miss
    } else {
        prompt_tokens.saturating_sub(cache_hit_tokens)
    };
    Usage {
        prompt_tokens,
        completion_tokens: number("completion_tokens"),
        reasoning_tokens: nested("completion_tokens_details", "reasoning_tokens"),
        cache_hit_tokens,
        cache_miss_tokens,
    }
}

/// Decodes a `finish_reason` string.
fn decode_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
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

    fn config(vendor: Vendor) -> OpenAiConfig {
        OpenAiConfig::new(vendor, "test-model", "test-key")
    }

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest::new("test-model", messages)
    }

    #[test]
    fn a_request_carries_streaming_and_usage_flags() {
        let body = build_request(&config(Vendor::OpenAi), &request(vec![Message::user("hi")]));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["model"], json!("test-model"));
    }

    #[test]
    fn dead_fields_are_never_sent() {
        let body = build_request(&config(Vendor::OpenAi), &request(vec![Message::user("hi")]));
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

    /// `OpenAI` takes the scale, so the effort travels as the step's own name.
    #[test]
    fn openai_sends_the_effort_scale() {
        let mut config = config(Vendor::OpenAi);
        for (effort, expected) in [
            (ReasoningEffort::None, "none"),
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            config.set_reasoning_effort(effort);
            let body = build_request(&config, &request(vec![Message::user("hi")]));
            assert_eq!(body["reasoning_effort"], json!(expected));
            // The other vendor's field is not sent to this one.
            assert!(body.get("thinking").is_none());
        }
    }

    /// z.ai takes the same scale but also needs the thinking switch named, and never disabled —
    /// its GLM-5.3 models refuse a disabled switch, so "off" is the `none` effort.
    #[test]
    fn zai_sends_a_thinking_switch_and_the_effort_scale() {
        let mut config = config(Vendor::Zai);
        config.set_reasoning_effort(ReasoningEffort::High);
        let body = build_request(&config, &request(vec![Message::user("hi")]));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));

        // The bottom of the scale is an effort rather than a disabled switch.
        config.set_reasoning_effort(ReasoningEffort::None);
        let body = build_request(&config, &request(vec![Message::user("hi")]));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("none"));
    }

    /// The budget sent is the capped one, and the cap is the provider's own.
    #[test]
    fn the_budget_sent_respects_the_provider_ceiling() {
        let mut zai = config(Vendor::Zai);
        assert!(zai.set_max_tokens(128_000).is_ok());
        let body = build_request(&zai, &request(vec![Message::user("hi")]));
        assert_eq!(body["max_tokens"], json!(98_304));
        assert_eq!(body["max_tokens"], json!(Vendor::Zai.max_output_tokens()));

        // The field's name is the vendor's as well: OpenAI's reasoning models require
        // `max_completion_tokens` and reject `max_tokens`.
        let mut openai = config(Vendor::OpenAi);
        assert!(openai.set_max_tokens(128_000).is_ok());
        let body = build_request(&openai, &request(vec![Message::user("hi")]));
        assert_eq!(body["max_completion_tokens"], json!(128_000));
        assert!(body.get("max_tokens").is_none(), "{body}");
    }

    #[test]
    fn an_empty_assistant_turn_sends_an_empty_string_not_null() {
        let message = Message::Assistant {
            text: None,
            reasoning: Some(String::from("because")),
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call-1"),
                name: tool_name("read"),
                arguments: json!({ "file_path": "a.txt" }),
            }],
        };
        let body = build_request(&config(Vendor::OpenAi), &request(vec![message]));
        let encoded = &body["messages"][0];
        assert_eq!(encoded["content"], json!(""));
        assert!(!encoded["content"].is_null());
        // Reasoning is written down but not replayed: OpenAI refuses a field it does
        // not know.
        assert!(encoded.get("reasoning_content").is_none());
    }

    #[test]
    fn tool_arguments_are_encoded_as_a_json_string() {
        let message = Message::Assistant {
            text: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call-7"),
                name: tool_name("bash"),
                arguments: json!({ "command": "ls" }),
            }],
        };
        let body = build_request(&config(Vendor::Zai), &request(vec![message]));
        let arguments = &body["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert!(arguments.is_string(), "arguments travel as a string");
        let parsed: Value =
            serde_json::from_str(arguments.as_str().unwrap_or("null")).unwrap_or_default();
        assert_eq!(parsed["command"], json!("ls"));
    }

    #[test]
    fn a_tool_result_uses_the_tool_role_and_call_id() {
        let message = Message::tool(ToolCallId::new("call-9"), "42", false);
        let body = build_request(&config(Vendor::OpenAi), &request(vec![message]));
        let encoded = &body["messages"][0];
        assert_eq!(encoded["role"], json!("tool"));
        assert_eq!(encoded["tool_call_id"], json!("call-9"));
        assert_eq!(encoded["content"], json!("42"));
    }

    #[test]
    fn only_the_three_schema_fields_reach_the_wire() {
        let schema = ToolSchema {
            name: tool_name("read"),
            description: String::from("Read a file"),
            parameters: json!({ "type": "object" }),
        };
        let tools = encode_tools(&[schema]);
        let Some(object) = tools[0]["function"].as_object() else {
            panic!("a function object is encoded");
        };
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["description", "name", "parameters"]);
    }

    /// The delta fold, both directions: the fields it understands become events,
    /// and the ones it does not are ignored rather than fatal.
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
        accumulator.observe_line(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
        );

        let mut events = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            events.push(event);
        }
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
            Some(String::from("read"))
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
    fn a_non_json_frame_becomes_an_error_event() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line("this is not json");
        assert!(matches!(accumulator.take_ready(), Some(LlmEvent::Error(_))));
    }

    /// A failure delivered inside a 200 response ends the stream with a reason,
    /// rather than as a stream that stops for no stated cause.
    #[test]
    fn an_error_frame_ends_the_stream_with_its_message() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(r#"{"error":{"message":"quota exceeded"}}"#);
        let event = accumulator.take_ready();
        assert!(
            matches!(&event, Some(LlmEvent::Error(message)) if message.contains("quota exceeded")),
            "{event:?}"
        );
        assert!(accumulator.is_closed());
    }

    #[test]
    fn finish_reasons_map_to_the_domain_vocabulary() {
        assert_eq!(decode_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(decode_finish_reason("tool_calls"), FinishReason::ToolCalls);
        // The older spelling of the same event, which some compatible servers send.
        assert_eq!(
            decode_finish_reason("function_call"),
            FinishReason::ToolCalls
        );
        assert_eq!(decode_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            decode_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
        assert_eq!(
            decode_finish_reason("something_new"),
            FinishReason::Unknown(String::from("something_new"))
        );
    }

    /// Both cache spellings are understood, and the miss counter is derived when it
    /// is not reported so the two counters still partition the prompt.
    #[test]
    fn usage_decoding_understands_both_cache_spellings() {
        let openai = decode_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 80 },
            "completion_tokens_details": { "reasoning_tokens": 30 }
        }));
        assert_eq!(openai.cache_hit_tokens, 80);
        assert_eq!(
            openai.cache_miss_tokens, 20,
            "derived from the prompt count"
        );
        assert_eq!(openai.reasoning_tokens, 30);

        let deepseek_shaped = decode_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 60,
            "prompt_cache_miss_tokens": 40
        }));
        assert_eq!(deepseek_shaped.cache_hit_tokens, 60);
        assert_eq!(deepseek_shaped.cache_miss_tokens, 40);

        // A response with no breakdown at all: zero, not an error.
        let bare = decode_usage(&json!({ "prompt_tokens": 3, "completion_tokens": 1 }));
        assert_eq!(bare.cache_hit_tokens, 0);
        assert_eq!(bare.reasoning_tokens, 0);
        assert_eq!(bare.total_tokens(), 4);
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
        assert!(matches!(
            accumulator.take_ready(),
            Some(LlmEvent::Finished { .. })
        ));
        accumulator.close();
        assert!(accumulator.take_ready().is_none());
        assert!(accumulator.is_closed());
    }

    /// A tool-call index sizes a vector, so an absurd one is folded rather than
    /// obeyed: the guard was `usize::try_from(u64)`, which cannot fail on a 64-bit
    /// target.
    #[test]
    fn an_absurd_tool_call_index_does_not_allocate_without_limit() {
        let mut assembled = StreamAccumulator::default();
        assembled.observe_tool_call_delta(&json!({
            "index": u64::MAX,
            "id": "c1",
            "type": "function",
            "function": { "name": "read", "arguments": "{}" }
        }));
        assert!(assembled.calls.len() <= MAX_TOOL_CALLS);
        assert_eq!(assembled.calls.len(), 1);
    }
}
