//! The `Anthropic` Messages wire protocol: request encoding and SSE decoding.
//!
//! This module is the only place that knows the Messages shapes, and they are not
//! the chat-completions shapes the other adapters send. Three differences matter
//! and each is a rule rather than a preference:
//!
//! - **A system turn is top-level.** There is no `system` role in `messages`; the
//!   instructions travel in a `system` field beside the conversation, so every
//!   system message is lifted out and joined. Sending one inside `messages` is
//!   refused by the API.
//! - **A tool result is a *user* turn.** The result of a tool call is a
//!   `tool_result` block in a message whose role is `user`, not a turn of its own
//!   with a `tool` role. Consecutive results are gathered into one turn, which is
//!   the shape the API documents and the one a model reads most clearly.
//! - **A tool call's arguments are an *object*.** `tool_use.input` is JSON, not a
//!   JSON string, so nothing has to be double-encoded and nothing has to be parsed
//!   on the way in or out.
//!
//! ## Streaming is event-typed rather than sentinel-terminated
//!
//! Every frame carries a `type`, and the stream ends at `message_stop` — there is no
//! `[DONE]`, which is why the decoder loop stops on the accumulator closing rather
//! than on a sentinel. Deltas name what they are: `text_delta`, `thinking_delta`,
//! and `input_json_delta`, the last carrying the fragment of a tool call's arguments.

use nanus_domain::{Message, ToolCallId, ToolName, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent};
use serde_json::{Map, Value, json};

use crate::config::AnthropicConfig;

/// The server-sent-events framer, shared with every adapter that streams this way.
pub use nanus_ports::SseFrames as SseDecoder;

/// Builds the JSON body for a message request.
#[must_use]
pub fn build_request(config: &AnthropicConfig, request: &ChatRequest) -> Value {
    // Precondition: a request without messages is meaningless, so it is asserted
    // here rather than sent and rejected upstream.
    assert!(
        !request.messages.is_empty(),
        "a chat request carries at least one message"
    );
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(config.model()));
    // Anthropic requires a ceiling on every request; there is no server default.
    body.insert(
        "max_tokens".to_owned(),
        json!(config.effective_max_tokens()),
    );
    body.insert("stream".to_owned(), json!(true));
    if let Some(system) = system_text(&request.messages) {
        body.insert("system".to_owned(), json!(system));
    }
    let messages = encode_messages(&request.messages);
    // Postcondition: a conversation is required. A request carrying only a system
    // prompt has nothing to answer, and the API refuses it.
    assert!(
        messages.as_array().is_some_and(|turns| !turns.is_empty()),
        "a request carries at least one conversational turn"
    );
    body.insert("messages".to_owned(), messages);
    if let Some(temperature) = request.temperature.or_else(|| config.temperature()) {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    // Only when a caller chose an effort: the API's own default is `high`, so an unset effort is
    // left out rather than sent as a value that only repeats the default. A model that takes no
    // effort parameter sends nothing whatever the caller chose.
    if let Some(effort) = request.reasoning_effort
        && let Some(spelling) = crate::config::effort_spelling(config.model(), effort)
    {
        body.insert("output_config".to_owned(), json!({ "effort": spelling }));
    }
    if !request.tools.is_empty() {
        body.insert("tools".to_owned(), encode_tools(&request.tools));
    }
    Value::Object(body)
}

/// Lifts the system instructions out of the conversation.
///
/// Several system messages are joined with a blank line, which is the closest thing
/// to "in this order" the top-level field can express.
#[must_use]
pub fn system_text(messages: &[Message]) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for message in messages {
        if let Message::System { text } = message {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n\n"))
}

/// Encodes the conversation in the Messages shape.
#[must_use]
pub fn encode_messages(messages: &[Message]) -> Value {
    let mut turns: Vec<Value> = Vec::new();
    // Tool results are gathered so that a step's several results become one user
    // turn rather than several; the API pairs them with the assistant turn that
    // asked, and one turn is the shape its own documentation uses.
    let mut results: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            // Lifted to the top level by `system_text`.
            Message::System { .. } => {}
            Message::Tool {
                call_id,
                content,
                is_error,
            } => {
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id.as_str(),
                    "content": content,
                    // Passed through rather than flattened into the text: the model
                    // is told a call failed, which is what lets it try something
                    // else rather than trusting a failure's wording.
                    "is_error": is_error,
                }));
            }
            Message::User { text } => {
                flush_results(&mut turns, &mut results);
                turns.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            Message::Assistant {
                text, tool_calls, ..
            } => {
                flush_results(&mut turns, &mut results);
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id.as_str(),
                        "name": call.name.as_str(),
                        // The arguments are an object here, not a JSON string.
                        "input": call.arguments,
                    }));
                }
                // Precondition: the domain's fold drops an empty assistant turn, and
                // the API refuses a content array with no blocks.
                assert!(
                    !blocks.is_empty(),
                    "an assistant turn carries text or tool calls"
                );
                turns.push(json!({ "role": "assistant", "content": blocks }));
            }
        }
    }
    flush_results(&mut turns, &mut results);
    Value::Array(turns)
}

/// Emits any gathered tool results as one user turn.
fn flush_results(turns: &mut Vec<Value>, results: &mut Vec<Value>) {
    if results.is_empty() {
        return;
    }
    let blocks = std::mem::take(results);
    turns.push(json!({ "role": "user", "content": blocks }));
}

/// Encodes the tool catalogue.
///
/// Only `name`, `description`, and `parameters` are placed on the wire — as
/// `input_schema`, which is this API's name for the same allowlist.
#[must_use]
pub fn encode_tools(tools: &[ToolSchema]) -> Value {
    let encoded: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name.as_str(),
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect();
    Value::Array(encoded)
}

/// Accumulates decoded frames into the events the agent loop consumes.
///
/// Unlike the chat-completions adapters this one does **not** hold tool calls until
/// the stream ends: a `content_block_start` names the call and its arguments follow
/// as fragments, so a call can be reported as it arrives and the ports crate's
/// assembler joins the pieces. What the accumulator holds is the accounting, because
/// the prompt count arrives at the start and the output count at the end.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    ready: Vec<LlmEvent>,
    input_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    output_tokens: u32,
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

    /// Returns `true` once the terminal event has been emitted.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
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
                truncate_for_message(payload)
            )));
            return;
        };
        self.observe_frame(&value);
    }

    /// Observes one decoded frame.
    ///
    /// An unrecognised `type` is ignored rather than fatal: the API is versioned by
    /// date and adds event kinds, and a client that refused an unknown one would
    /// break on a release note.
    pub fn observe_frame(&mut self, frame: &Value) {
        match frame.get("type").and_then(Value::as_str) {
            Some("message_start") => self.observe_message_start(frame),
            Some("content_block_start") => self.observe_block_start(frame),
            Some("content_block_delta") => self.observe_block_delta(frame),
            Some("message_delta") => self.observe_message_delta(frame),
            Some("message_stop") => self.close(),
            Some("error") => {
                let message = frame
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map_or_else(|| frame.to_string(), str::to_owned);
                self.fail(format!("the provider reported an error: {message}"));
            }
            // `ping` keep-alives, `content_block_stop`, and anything a later version
            // adds: nothing the harness acts on.
            _ => {}
        }
    }

    /// Reads the prompt accounting, which arrives once with the message head.
    fn observe_message_start(&mut self, frame: &Value) {
        let Some(usage) = frame
            .get("message")
            .and_then(|message| message.get("usage"))
        else {
            return;
        };
        self.input_tokens = number(usage, "input_tokens").max(self.input_tokens);
        self.cache_read_tokens =
            number(usage, "cache_read_input_tokens").max(self.cache_read_tokens);
        self.cache_creation_tokens =
            number(usage, "cache_creation_input_tokens").max(self.cache_creation_tokens);
        self.output_tokens = number(usage, "output_tokens").max(self.output_tokens);
    }

    /// Reads the opening of a content block: text, thinking, or a tool call.
    fn observe_block_start(&mut self, frame: &Value) {
        let index = block_index(frame);
        let Some(block) = frame.get("content_block") else {
            return;
        };
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => self.open_tool_use(index, block),
            Some("text") => {
                if let Some(text) = non_empty_str(block, "text") {
                    self.ready.push(LlmEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking") => {
                if let Some(text) = non_empty_str(block, "thinking") {
                    self.ready.push(LlmEvent::ReasoningDelta(text.to_owned()));
                }
            }
            _ => {}
        }
    }

    /// Opens one tool call from a `tool_use` block.
    ///
    /// The id and the name are what the ports crate's assembler joins the argument
    /// fragments to, so a block missing either is reported rather than opened: an
    /// unnamed call can never be dispatched.
    fn open_tool_use(&mut self, index: u32, block: &Value) {
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.is_empty() {
            self.ready.push(LlmEvent::Error(format!(
                "the model opened tool call {index} with no id"
            )));
            return;
        }
        let Ok(name) = ToolName::new(name) else {
            self.ready.push(LlmEvent::Error(format!(
                "the model requested a tool whose name is not usable: {:?}",
                truncate_for_message(name)
            )));
            return;
        };
        self.ready.push(LlmEvent::ToolCallDelta {
            index,
            id: Some(ToolCallId::new(id)),
            name: Some(name),
            // The arguments follow as fragments; the opening carries none.
            arguments_delta: String::new(),
        });
    }

    /// Reads one delta inside a content block.
    fn observe_block_delta(&mut self, frame: &Value) {
        let index = block_index(frame);
        let Some(delta) = frame.get("delta") else {
            return;
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = non_empty_str(delta, "text") {
                    self.ready.push(LlmEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = non_empty_str(delta, "thinking") {
                    self.ready.push(LlmEvent::ReasoningDelta(text.to_owned()));
                }
            }
            Some("input_json_delta") => {
                // A fragment of a tool call's arguments, joined by the assembler.
                if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                    self.ready.push(LlmEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: fragment.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Reads the tail: the stop reason and the generated token count.
    fn observe_message_delta(&mut self, frame: &Value) {
        if let Some(usage) = frame.get("usage") {
            self.output_tokens = number(usage, "output_tokens").max(self.output_tokens);
        }
        let reason = frame
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str);
        if let Some(reason) = reason {
            self.finish = Some(decode_stop_reason(reason));
        }
    }

    /// Emits the accounting and the terminal event.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let usage = Usage::new(
            self.prompt_tokens(),
            self.output_tokens,
            // Anthropic does not break its output down into thinking and answering,
            // and thinking is never requested here, so there is no share to report.
            0,
            self.cache_read_tokens,
            // A miss is everything the cache did not serve: the uncached input *and*
            // the tokens just written into the cache. The latter are not hits — they
            // were not served from a cache — and the two counters have to partition
            // the prompt, which is the invariant the harness's accounting relies on.
            self.input_tokens.saturating_add(self.cache_creation_tokens),
        );
        if usage.total_tokens() > 0 {
            self.ready.push(LlmEvent::Usage(usage));
        }
        let reason = self.finish.take().unwrap_or(FinishReason::Stop);
        self.ready.push(LlmEvent::Finished { reason });
    }

    /// Returns the whole prompt: uncached input plus both cache counts.
    const fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }
}

/// Returns a frame's content block index.
///
/// The index is a join key rather than a size, so an absurd value is saturated
/// rather than folded into zero: folding would make two different blocks collide,
/// and a key in a map allocates nothing.
fn block_index(frame: &Value) -> u32 {
    frame
        .get("index")
        .and_then(Value::as_u64)
        .map_or(0, |index| u32::try_from(index).unwrap_or(u32::MAX))
}

/// Returns a numeric field's value, or zero when it is absent.
fn number(value: &Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or_default()
}

/// Returns a string field's value when it is present and non-empty.
fn non_empty_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Decodes a `stop_reason`.
fn decode_stop_reason(raw: &str) -> FinishReason {
    match raw {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        // `pause_turn` and anything a later version adds: preserved rather than
        // guessed at, so a transcript still says what happened.
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
    use nanus_domain::ToolCall;

    fn config() -> AnthropicConfig {
        AnthropicConfig::new("claude-sonnet-4-20250514", "test-key")
    }

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest::new("claude-sonnet-4-20250514", messages)
    }

    /// The three rules the Messages API imposes, in one body.
    #[test]
    fn a_request_lifts_the_system_turn_and_keeps_the_conversation() {
        let messages = vec![
            Message::system("you are nanus"),
            Message::user("hi"),
            Message::assistant(
                Some(String::from("ok")),
                None,
                vec![ToolCall::new(
                    ToolCallId::new("toolu_1"),
                    tool_name("read"),
                    json!({ "path": "a.txt" }),
                )],
            ),
            Message::tool(ToolCallId::new("toolu_1"), "contents", false),
        ];
        let body = build_request(&config(), &request(messages));
        assert_eq!(body["system"], json!("you are nanus"));
        let turns = body["messages"].as_array().cloned().unwrap_or_default();
        assert_eq!(turns.len(), 3, "{turns:?}");
        // The system turn is not among them.
        assert!(
            turns.iter().all(|turn| turn["role"] != json!("system")),
            "{turns:?}"
        );
        // The tool call's arguments are an object, not a JSON string.
        let call = &turns[1]["content"][1];
        assert_eq!(call["type"], json!("tool_use"));
        assert_eq!(call["input"]["path"], json!("a.txt"));
        // The tool result is a user turn with a tool_result block.
        assert_eq!(turns[2]["role"], json!("user"));
        assert_eq!(turns[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(turns[2]["content"][0]["tool_use_id"], json!("toolu_1"));
    }

    /// Nothing is sent that the API does not define, and the required ceiling is.
    #[test]
    fn a_request_carries_the_required_ceiling_and_no_dead_fields() {
        let body = build_request(&config(), &request(vec![Message::user("hi")]));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["max_tokens"], json!(crate::config::MAX_OUTPUT_TOKENS));
        assert!(body.get("system").is_none(), "no system turn was sent");
        for field in [
            "presence_penalty",
            "frequency_penalty",
            "logprobs",
            "response_format",
            "tool_choice",
            "seed",
            "stream_options",
            "reasoning_effort",
            "thinking",
        ] {
            assert!(body.get(field).is_none(), "{field} must not be sent");
        }
    }

    /// The budget sent is capped at the model's documented ceiling.
    #[test]
    fn the_budget_sent_respects_the_ceiling() {
        let mut config = config();
        assert!(config.set_max_tokens(128_000).is_ok());
        let body = build_request(&config, &request(vec![Message::user("hi")]));
        assert_eq!(body["max_tokens"], json!(64_000));
    }

    /// Several system turns join; several tool results gather into one user turn.
    #[test]
    fn system_turns_join_and_tool_results_gather() {
        let messages = vec![
            Message::system("first"),
            Message::system("second"),
            Message::assistant(
                None,
                None,
                vec![
                    ToolCall::new(ToolCallId::new("a"), tool_name("read"), json!({})),
                    ToolCall::new(ToolCallId::new("b"), tool_name("glob"), json!({})),
                ],
            ),
            Message::tool(ToolCallId::new("a"), "one", false),
            Message::tool(ToolCallId::new("b"), "two", true),
        ];
        let body = build_request(&config(), &request(messages));
        assert_eq!(body["system"], json!("first\n\nsecond"));
        let turns = body["messages"].as_array().cloned().unwrap_or_default();
        // Two turns, not three: both results are one user turn.
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[1]["role"], json!("user"));
        let blocks = turns[1]["content"].as_array().cloned().unwrap_or_default();
        assert_eq!(blocks.len(), 2);
        // The failure is passed through, which is what lets the model react to it.
        assert_eq!(blocks[1]["is_error"], json!(true));
    }

    #[test]
    fn only_the_three_schema_fields_reach_the_wire() {
        let schema = ToolSchema {
            name: tool_name("read"),
            description: String::from("Read a file"),
            parameters: json!({ "type": "object" }),
        };
        let tools = encode_tools(&[schema]);
        let Some(object) = tools[0].as_object() else {
            panic!("a tool object is encoded");
        };
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["description", "input_schema", "name"]);
    }

    /// A whole streamed message: text, a tool call built from fragments, usage, and
    /// the stop reason.
    #[test]
    fn a_streamed_message_becomes_events() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        );
        accumulator.observe_line(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
        );
        accumulator.observe_line(r#"{"type":"content_block_stop","index":0}"#);
        accumulator.observe_line(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        );
        accumulator.observe_line(r#"{"type":"message_stop"}"#);

        let mut events = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            events.push(event);
        }
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        // The call is opened with its id and name, then extended by fragments; the
        // ports crate's assembler joins them.
        let mut assembler = nanus_ports::ToolCallAssembler::new();
        for event in &events {
            assert!(assembler.apply_event(event).is_ok());
        }
        let calls = assembler.finish();
        assert!(calls.is_ok(), "{calls:?}");
        let calls = calls.unwrap_or_default();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls
                .first()
                .map(|call| call.arguments.get("path").cloned()),
            Some(Some(json!("a.txt")))
        );
        // The prompt is the whole prompt, and the cache counters partition it.
        let usage = events.iter().find_map(|event| match event {
            LlmEvent::Usage(usage) => Some(*usage),
            _ => None,
        });
        let usage = usage.unwrap_or_default();
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.cache_hit_tokens, 4);
        // Ten uncached tokens plus six written into the cache: a miss is everything
        // the cache did not serve.
        assert_eq!(usage.cache_miss_tokens, 16);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(
            usage
                .cache_hit_tokens
                .saturating_add(usage.cache_miss_tokens),
            usage.prompt_tokens
        );
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Finished {
                reason: FinishReason::ToolCalls
            })
        ));
        assert!(accumulator.is_closed());
    }

    /// The terminal event is emitted exactly once, whether the stream ends at
    /// `message_stop` or at the socket.
    #[test]
    fn closing_twice_emits_one_terminal_event() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_frame(&json!({ "type": "message_stop" }));
        assert!(matches!(
            accumulator.take_ready(),
            Some(LlmEvent::Finished { .. })
        ));
        accumulator.close();
        assert!(accumulator.take_ready().is_none());
        assert!(accumulator.is_closed());
    }

    /// A stream with no usage reported emits no usage event, rather than a zero one
    /// that would look like a measured figure.
    #[test]
    fn no_usage_is_reported_when_none_arrived() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        accumulator.close();
        let mut events = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            events.push(event);
        }
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Usage(_))),
            "{events:?}"
        );
        assert!(matches!(events.last(), Some(LlmEvent::Finished { .. })));
    }

    /// An error frame ends the stream with the provider's message.
    #[test]
    fn an_error_frame_ends_the_stream() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
        );
        let event = accumulator.take_ready();
        assert!(
            matches!(&event, Some(LlmEvent::Error(message)) if message.contains("overloaded")),
            "{event:?}"
        );
        assert!(accumulator.is_closed());
    }

    /// An unknown event is ignored rather than fatal: the API adds kinds.
    #[test]
    fn an_unknown_event_is_ignored() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(r#"{"type":"ping"}"#);
        accumulator.observe_line(r#"{"type":"something_new","detail":"x"}"#);
        assert!(accumulator.take_ready().is_none());
        assert!(!accumulator.is_closed());
    }

    /// A tool call with no id or no usable name is reported rather than opened,
    /// because the assembler could never join its fragments.
    #[test]
    fn a_call_with_no_id_or_name_is_reported() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"read","input":{}}}"#,
        );
        let event = accumulator.take_ready();
        assert!(matches!(event, Some(LlmEvent::Error(_))), "{event:?}");

        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_line(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"not a name","input":{}}}"#,
        );
        let event = accumulator.take_ready();
        assert!(matches!(event, Some(LlmEvent::Error(_))), "{event:?}");
    }

    #[test]
    fn every_stop_reason_has_an_answer() {
        assert_eq!(decode_stop_reason("end_turn"), FinishReason::Stop);
        assert_eq!(decode_stop_reason("stop_sequence"), FinishReason::Stop);
        assert_eq!(decode_stop_reason("tool_use"), FinishReason::ToolCalls);
        assert_eq!(decode_stop_reason("max_tokens"), FinishReason::Length);
        assert_eq!(decode_stop_reason("refusal"), FinishReason::ContentFilter);
        // Preserved rather than mapped to a clean finish.
        assert_eq!(
            decode_stop_reason("pause_turn"),
            FinishReason::Unknown(String::from("pause_turn"))
        );
    }

    /// A content block's index is the join key for its fragments, and an absurd
    /// value does not allocate anything.
    #[test]
    fn an_absurd_block_index_is_bounded() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_frame(&json!({
            "type": "content_block_delta",
            "index": u64::MAX,
            "delta": { "type": "input_json_delta", "partial_json": "{}" }
        }));
        let event = accumulator.take_ready();
        assert!(
            matches!(
                event,
                Some(LlmEvent::ToolCallDelta {
                    index: u32::MAX,
                    ..
                })
            ),
            "{event:?}"
        );
    }
}
