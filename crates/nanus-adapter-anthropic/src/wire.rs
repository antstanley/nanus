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

use core::fmt::Write as _;
use sha2::{Digest as _, Sha256};

use nanus_domain::{Message, ToolCallId, ToolName, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent};
use serde_json::{Map, Value, json};

use crate::config::AnthropicConfig;

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
    // The request's id rather than the adapter's configured one: a runner switches models by naming
    // a different id in the request, without rebuilding the adapter. The effort gate below reads the
    // same id, because which models take `output_config.effort` is a fact about the model being
    // asked, not about the one the adapter was built for.
    body.insert("model".to_owned(), json!(request.model));
    // Anthropic requires a ceiling on every request; there is no server default.
    body.insert(
        "max_tokens".to_owned(),
        json!(
            request
                .max_tokens
                .unwrap_or_else(|| config.max_tokens())
                .min(crate::config::model_max_output_tokens(&request.model))
        ),
    );
    body.insert("stream".to_owned(), json!(true));
    if matches!(
        request.model.as_str(),
        "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-fable-5-1"
    ) {
        body.insert("thinking".to_owned(), json!({ "type": "adaptive" }));
    }
    if let Some(system) = system_text(&request.messages) {
        body.insert("system".to_owned(), json!(system));
    }
    let tools = if request.tools.is_empty() {
        Value::Null
    } else {
        encode_tools(&request.tools)
    };
    let system = body.get("system").cloned().unwrap_or(Value::Null);
    let messages = encode_messages_with_prefix(&request.messages, &system, &tools);
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
        && let Some(spelling) = crate::config::effort_spelling(&request.model, effort)
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

/// Fingerprints only the fields Anthropic binds signed thinking to.
pub fn request_prefix(payload: &Value) -> String {
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    prefix_fingerprint(&payload["system"], &payload["tools"], &messages)
}

fn prefix_fingerprint(system: &Value, tools: &Value, messages: &[Value]) -> String {
    let prefix = json!({ "system": system, "tools": tools, "messages": messages });
    let raw = prefix.to_string();
    let digest = Sha256::digest(raw.as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// Preserves typed result order inside the matching tool-result block.
fn encode_content(blocks: &[nanus_domain::ContentBlock]) -> Value {
    Value::Array(blocks.iter().map(|block| match block {
        nanus_domain::ContentBlock::Text(text) => json!({ "type": "text", "text": text }),
        nanus_domain::ContentBlock::Image { media_type, data_base64 } => json!({
            "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data_base64 },
        }),
    }).collect())
}

fn encode_messages_with_prefix(messages: &[Message], system: &Value, tools: &Value) -> Value {
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
                content_blocks,
                call_id,
                content,
                is_error,
            } => {
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id.as_str(),
                    "content": content_blocks.as_ref().map_or_else(|| json!(content), |blocks| {
                        if blocks.iter().any(|block| matches!(block, nanus_domain::ContentBlock::Image { .. })) {
                            encode_content(blocks)
                        } else { json!(message.tool_text()) }
                    }),
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
                text,
                tool_calls,
                replay,
                ..
            } => {
                flush_results(&mut turns, &mut results);
                if let Some(replay) = replay
                    && replay
                        .validate_response(text.as_deref(), tool_calls)
                        .is_ok()
                    && replay.protocol == "anthropic.messages"
                    && replay.prefix_digest == prefix_fingerprint(system, tools, &turns)
                {
                    turns.push(json!({ "role": "assistant", "content": replay.blocks }));
                    continue;
                }
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in tool_calls {
                    // The arguments are an object here, not a JSON string. A call whose
                    // arguments never parsed is logged as the raw string so the registry
                    // can say why it was refused; the API refuses a non-object `input`
                    // and would refuse every later request in the session, so the
                    // replay carries an empty object instead.
                    let input = if call.arguments.is_object() {
                        call.arguments.clone()
                    } else {
                        json!({})
                    };
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id.as_str(),
                        "name": call.name.as_str(),
                        "input": input,
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
    limits: Option<nanus_ports::ResponseLimits>,
    ready: Vec<LlmEvent>,
    input_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    output_tokens: u32,
    finish: Option<FinishReason>,
    closed: bool,
    prefix_digest: String,
    replay_blocks: std::collections::BTreeMap<u32, Value>,
    replay_arguments: std::collections::BTreeMap<u32, String>,
    replay_bytes: usize,
}

impl StreamAccumulator {
    /// Installs immutable caller budgets before observing any frame.
    pub fn set_response_limits(&mut self, limits: Option<nanus_ports::ResponseLimits>) {
        self.limits = limits;
    }

    /// Starts a stream whose signed content is bound to this exact request prefix.
    pub fn with_prefix(prefix_digest: String) -> Self {
        Self {
            prefix_digest,
            ..Self::default()
        }
    }

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
        if self.limits.is_some() {
            self.ready.clear();
            self.replay_blocks.clear();
            self.replay_arguments.clear();
            self.finish = None;
        }
        self.ready.push(LlmEvent::Error(message));
        self.closed = true;
    }

    /// Observes one decoded SSE payload.
    pub fn observe_line(&mut self, payload: &str) {
        if self.limits.is_some() && self.closed {
            return;
        }
        let parsed: Result<Value, _> = serde_json::from_str(payload);
        let Ok(value) = parsed else {
            if self.limits.is_some() {
                self.fail("malformed stream: provider payload is not JSON".into());
                return;
            }
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
        if let Some(limits) = self.limits {
            if self.closed {
                return;
            }
            if let Err(error) = limits.frame(frame) {
                self.fail(error.to_string());
                return;
            }
        }
        if let Some(limits) = self.limits
            && let Err(error) = self.check_replay_frame(frame, limits)
        {
            self.fail(error.to_string());
            return;
        }
        self.observe_replay(frame);
        if self.closed {
            return;
        }
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

    fn check_replay_frame(
        &self,
        frame: &Value,
        limits: nanus_ports::ResponseLimits,
    ) -> nanus_ports::LlmResult<()> {
        if let Some(index) = frame.get("index") {
            let index = index
                .as_u64()
                .ok_or_else(|| nanus_ports::LlmError::MalformedStream {
                    message: "invalid assistant block index".into(),
                })?;
            limits.index(index)?;
        }
        let added = match frame.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                if self.replay_blocks.len() >= limits.tool_slots() {
                    return Err(nanus_ports::LlmError::ResponseLimit {
                        resource: "assistant block slots",
                        limit: limits.tool_slots(),
                    });
                }
                frame
                    .get("content_block")
                    .map(|block| {
                        nanus_domain::content::serialized_size(block, limits.event_bytes())
                    })
                    .transpose()
                    .map_err(|_| nanus_ports::LlmError::ResponseLimit {
                        resource: "assistant replay bytes",
                        limit: limits.event_bytes(),
                    })?
                    .unwrap_or(0)
            }
            Some("content_block_delta") => frame.get("delta").map_or(0, |delta| {
                let field = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => "text",
                    Some("thinking_delta") => "thinking",
                    Some("signature_delta") => "signature",
                    Some("input_json_delta") => "partial_json",
                    _ => return 0,
                };
                delta.get(field).and_then(Value::as_str).map_or(0, str::len)
            }),
            _ => 0,
        };
        nanus_ports::ResponseLimits::add(
            "assistant replay bytes",
            self.replay_bytes,
            added,
            limits.event_bytes(),
        )?;
        Ok(())
    }

    /// Reassembles original blocks, including signatures that carry no visible text.
    fn observe_replay(&mut self, frame: &Value) {
        let index = block_index(frame);
        match frame.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                if let Some(block) = frame.get("content_block") {
                    // Every later write assigns a field of the stored block, and assigning
                    // a field of a string or an array panics. Refusing anything but an
                    // object here, the one place a block is stored, keeps that true.
                    if !block.is_object() {
                        self.fail("malformed stream: assistant block is not an object".into());
                        return;
                    }
                    if self.replay_blocks.len() >= 256 {
                        self.fail("more than 256 assistant blocks".into());
                        return;
                    }
                    let Ok(bytes) = nanus_domain::content::serialized_size(
                        block,
                        nanus_domain::content::RECORD_BYTES_MAX,
                    ) else {
                        self.fail("assistant block exceeds record byte limit".into());
                        return;
                    };
                    self.replay_bytes = self.replay_bytes.saturating_add(bytes);
                    if self.replay_bytes > nanus_domain::content::RECORD_BYTES_MAX {
                        self.fail("assistant replay exceeds record byte limit".into());
                        return;
                    }
                    self.replay_blocks.insert(index, block.clone());
                }
            }
            Some("content_block_delta") => {
                if let Some(delta) = frame.get("delta") {
                    self.absorb_replay_delta(index, delta);
                }
            }
            Some("content_block_stop") => {
                if let Some(raw) = self.replay_arguments.remove(&index) {
                    // The API opens a call with an empty `partial_json` delta, and a
                    // tool that takes no arguments never sends another: empty means
                    // "no arguments", exactly as the assembler and the loop read it.
                    let raw = if raw.trim().is_empty() {
                        "{}"
                    } else {
                        raw.as_str()
                    };
                    match serde_json::from_str::<Value>(raw) {
                        Ok(input) => {
                            if let Some(block) = self.replay_blocks.get_mut(&index) {
                                block["input"] = input;
                            }
                        }
                        Err(error) => {
                            self.fail(format!("invalid streamed tool arguments: {error}"));
                        }
                    }
                }
            }
            _ => {}
        }
        if self.replay_bytes > nanus_domain::content::RECORD_BYTES_MAX {
            self.fail("assistant replay exceeds record byte limit".into());
        }
    }

    fn absorb_replay_delta(&mut self, index: u32, delta: &Value) {
        let field = match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => "text",
            Some("thinking_delta") => "thinking",
            Some("signature_delta") => "signature",
            Some("input_json_delta") => "partial_json",
            _ => return,
        };
        let Some(fragment) = delta.get(field).and_then(Value::as_str) else {
            return;
        };
        self.replay_bytes = self.replay_bytes.saturating_add(fragment.len());
        if self.replay_bytes > nanus_domain::content::RECORD_BYTES_MAX {
            self.fail("assistant replay exceeds record byte limit".into());
            return;
        }
        if field == "partial_json" {
            self.replay_arguments
                .entry(index)
                .or_default()
                .push_str(fragment);
        } else if let Some(block) = self.replay_blocks.get_mut(&index) {
            // Appended in place. Copying the text so far to append one fragment made a
            // reply cost the square of its length in allocation.
            match block.get_mut(field) {
                Some(Value::String(text)) => text.push_str(fragment),
                _ => block[field] = Value::String(fragment.to_owned()),
            }
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
            if self.limits.is_some() {
                self.fail("malformed stream: tool call has no id".into());
                return;
            }
            self.ready.push(LlmEvent::Error(format!(
                "the model opened tool call {index} with no id"
            )));
            return;
        }
        let Ok(name) = ToolName::new(name) else {
            if self.limits.is_some() {
                self.fail("malformed stream: tool call has invalid name".into());
                return;
            }
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
        let blocks: Vec<_> = core::mem::take(&mut self.replay_blocks)
            .into_values()
            .collect();
        if blocks.iter().any(|block| {
            matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        }) {
            if blocks.iter().any(|block| {
                block["type"] == "thinking" && block["signature"].as_str().is_none_or(str::is_empty)
            }) {
                self.fail("thinking block has no completed signature".into());
                return;
            }
            let replay = nanus_domain::message::AssistantReplay {
                protocol: "anthropic.messages".into(),
                prefix_digest: self.prefix_digest.clone(),
                blocks,
            };
            if let Some(limits) = self.limits
                && nanus_domain::content::serialized_size(&replay, limits.event_bytes()).is_err()
            {
                self.fail(
                    nanus_ports::LlmError::ResponseLimit {
                        resource: "assistant replay bytes",
                        limit: limits.event_bytes(),
                    }
                    .to_string(),
                );
                return;
            }
            self.ready.push(LlmEvent::AssistantReplay(replay));
        }
        let usage = Usage::new(
            self.prompt_tokens(),
            self.output_tokens,
            // Anthropic does not break its output down into thinking and answering,
            // so there is no separately reported share to record.
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
    use nanus_ports::ReasoningEffort;

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

    /// A call whose arguments never parsed is logged as a raw string; replaying it as
    /// `input` would make the API refuse every later request in the session.
    #[test]
    fn a_tool_call_with_unparsed_arguments_replays_as_an_empty_object() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant(
                None,
                None,
                vec![
                    ToolCall::new(
                        ToolCallId::new("bad"),
                        tool_name("read"),
                        json!("{not json"),
                    ),
                    ToolCall::new(
                        ToolCallId::new("good"),
                        tool_name("read"),
                        json!({ "p": 1 }),
                    ),
                ],
            ),
        ];
        let body = build_request(&config(), &request(messages));
        let blocks = &body["messages"][1]["content"];
        assert_eq!(blocks[0]["input"], json!({}), "{blocks:?}");
        assert_eq!(blocks[1]["input"], json!({ "p": 1 }), "{blocks:?}");
    }

    /// A tool that takes no arguments opens with an empty `partial_json` delta and may
    /// send nothing else; that is "no arguments", not a failed response.
    #[test]
    fn a_streamed_call_with_only_an_empty_argument_delta_is_not_an_error() {
        let mut accumulator = StreamAccumulator::default();
        for frame in [
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "c", "name": "get_goal", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ] {
            accumulator.observe_frame(&frame);
        }
        while let Some(event) = accumulator.take_ready() {
            assert!(!matches!(event, LlmEvent::Error(_)), "{event:?}");
        }
        // And the other direction: a fragment that is not JSON still fails.
        let mut accumulator = StreamAccumulator::default();
        for frame in [
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "c", "name": "read", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{oops" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ] {
            accumulator.observe_frame(&frame);
        }
        let mut failed = false;
        while let Some(event) = accumulator.take_ready() {
            failed |= matches!(event, LlmEvent::Error(_));
        }
        assert!(failed, "malformed streamed arguments must still fail");
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
            "output_config",
        ] {
            assert!(body.get(field).is_none(), "{field} must not be sent");
        }
    }

    /// Effort reaches the wire as `output_config.effort`, and only for a model that takes it.
    ///
    /// The model the *request* names is the one the gate reads, so each case builds its own request
    /// rather than reusing one whose id would decide the answer instead of the configured model.
    #[test]
    fn effort_reaches_the_wire_only_where_the_model_takes_it() {
        let modern = AnthropicConfig::new("claude-sonnet-5", "test-key");
        for (effort, expected) in [
            (ReasoningEffort::None, "low"),
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            let body = build_request(
                &modern,
                &ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
                    .with_reasoning_effort(effort),
            );
            assert_eq!(
                body["output_config"]["effort"],
                json!(expected),
                "the chosen step travels as its own name: {effort:?}"
            );
        }

        // A model that predates the parameter is sent nothing, whatever the caller chose.
        let legacy = AnthropicConfig::new("claude-haiku-4-5-20251001", "test-key");
        let body = build_request(
            &legacy,
            &ChatRequest::new("claude-haiku-4-5-20251001", vec![Message::user("hi")])
                .with_reasoning_effort(ReasoningEffort::Max),
        );
        assert!(body.get("output_config").is_none(), "{body}");

        // And an unset effort is left out entirely, so the API's own default applies.
        let body = build_request(
            &modern,
            &ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")]),
        );
        assert!(body.get("output_config").is_none(), "{body}");
    }

    /// A switched model reaches the wire, and the effort gate follows the request's id rather than
    /// the adapter's configured one: a runner switches models without rebuilding the adapter, so the
    /// configured id must not decide which model is asked or whether it takes `output_config`.
    #[test]
    fn a_switched_model_decides_the_body_and_the_effort_gate() {
        // Built for a legacy model, asked for a modern one: the modern id travels and takes effort.
        let legacy = AnthropicConfig::new("claude-haiku-4-5-20251001", "test-key");
        let body = build_request(
            &legacy,
            &ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
                .with_reasoning_effort(ReasoningEffort::High),
        );
        assert_eq!(body["model"], json!("claude-sonnet-5"), "{body}");
        assert_eq!(body["output_config"]["effort"], json!("high"), "{body}");

        // Built for a modern model, asked for a legacy one: the legacy id travels and takes nothing,
        // even though the adapter was configured for a model that does.
        let modern = AnthropicConfig::new("claude-sonnet-5", "test-key");
        let body = build_request(
            &modern,
            &ChatRequest::new("claude-haiku-4-5-20251001", vec![Message::user("hi")])
                .with_reasoning_effort(ReasoningEffort::High),
        );
        assert_eq!(body["model"], json!("claude-haiku-4-5-20251001"), "{body}");
        assert!(body.get("output_config").is_none(), "{body}");
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
    #[test]
    fn signed_empty_thinking_survives_streaming_reload_and_unchanged_prefix_replay() {
        for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
            let config = AnthropicConfig::new(model, "fixture-key");
            let initial = ChatRequest::new(model, vec![Message::user("Inspect")]);
            let payload = build_request(&config, &initial);
            assert_eq!(payload["thinking"], json!({ "type": "adaptive" }));
            let mut accumulator = StreamAccumulator::with_prefix(request_prefix(&payload));
            for frame in [
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "opaque-" } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "signature" } }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "call-1", "name": "inspect", "input": {} } }),
                json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"fixture.png\"}" } }),
                json!({ "type": "content_block_stop", "index": 1 }),
                json!({ "type": "message_stop" }),
            ] {
                accumulator.observe_frame(&frame);
            }
            let mut replay = None;
            while let Some(event) = accumulator.take_ready() {
                if let LlmEvent::AssistantReplay(blocks) = event {
                    replay = Some(blocks);
                }
            }
            let replay = replay.expect("signed blocks retained");
            assert_eq!(
                replay.blocks[0],
                json!({ "type": "thinking", "thinking": "", "signature": "opaque-signature" })
            );
            assert_eq!(replay.blocks[1]["input"], json!({ "path": "fixture.png" }));
            let mut session =
                nanus_domain::Session::new(nanus_domain::SessionId::new("signed"), 123, "/caller");
            session.append(nanus_domain::SessionEvent::UserMessage {
                text: "Inspect".into(),
            });
            let call = ToolCall::new(
                ToolCallId::new("call-1"),
                ToolName::new("inspect").unwrap(),
                json!({ "path": "fixture.png" }),
            );
            session.append(nanus_domain::SessionEvent::AssistantMessage {
                replay: Some(replay.clone()),
                text: None,
                reasoning: None,
                tool_calls: vec![call],
                usage: None,
                interrupted: false,
                model: Some(model.into()),
                effort: None,
            });
            session.append(nanus_domain::SessionEvent::ToolResult {
                call_id: ToolCallId::new("call-1"),
                content: "inspected".into(),
                content_blocks: None,
                is_error: false,
            });
            let loaded =
                nanus_domain::Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap();
            let request = ChatRequest::new(model, loaded.derive_messages());
            let next = build_request(&config, &request);
            assert_eq!(next["messages"][1]["content"], json!(replay.blocks));
            let mut edited = request;
            edited
                .messages
                .insert(0, Message::system("A changed instruction"));
            let changed = build_request(&config, &edited);
            assert_eq!(changed["messages"][1]["content"][0]["type"], "tool_use");
            assert!(!changed.to_string().contains("opaque-signature"));
        }
    }

    #[test]
    fn replay_text_is_joined_whether_or_not_the_block_opened_with_it() {
        // The thinking block opens with its fields and the text block without its own, so
        // one grows a string the start carried and the other has to start one.
        let mut accumulator = StreamAccumulator::default();
        for frame in [
            json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
            json!({ "type": "content_block_start", "index": 1,
                "content_block": { "type": "text" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "thinking_delta", "thinking": "one " } }),
            json!({ "type": "content_block_delta", "index": 1,
                "delta": { "type": "text_delta", "text": "first" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "thinking_delta", "thinking": "two" } }),
            json!({ "type": "content_block_delta", "index": 1,
                "delta": { "type": "text_delta", "text": " second" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "signature_delta", "signature": "signed" } }),
            json!({ "type": "message_stop" }),
        ] {
            accumulator.observe_frame(&frame);
        }
        let mut replay = None;
        while let Some(event) = accumulator.take_ready() {
            if let LlmEvent::AssistantReplay(blocks) = event {
                replay = Some(blocks);
            }
        }
        let replay = replay.expect("signed blocks retained");
        assert_eq!(
            replay.blocks[0],
            json!({ "type": "thinking", "thinking": "one two", "signature": "signed" })
        );
        assert_eq!(
            replay.blocks[1],
            json!({ "type": "text", "text": "first second" })
        );
    }

    #[test]
    fn incomplete_thinking_is_refused_and_does_not_become_replay() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe_frame(&json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "thinking", "thinking": "", "signature": "" } }));
        accumulator.close();
        assert!(
            matches!(accumulator.take_ready(), Some(LlmEvent::Error(message)) if message.contains("signature"))
        );
    }
}

#[cfg(test)]
#[path = "wire_bounds_tests.rs"]
mod bounds_tests;
