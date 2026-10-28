//! The Responses API: the wire the `ChatGPT` subscription speaks.
//!
//! A subscription-backed request is not a chat completion. The conversation goes as a flat list of
//! *items* — a user turn, the assistant's output, the function calls it made, and their results —
//! with the system prompt as the first item, a `developer` message with one `input_text` part per
//! system message, and the reasoning control moved under a `reasoning` object. Streaming is a
//! sequence of named events rather than a stream of `choices` deltas, so the decoder dispatches on
//! each payload's own `type`.
//!
//! The system prompt is a developer message rather than the top-level `instructions` field because
//! a part can carry an explicit cache breakpoint and `instructions` cannot: on the API plan the
//! harness's prompt, the first part, is marked as an entry every session with the same tools and
//! prompt shares. The opt-in stateless replay mode is the one that keeps `instructions`, because
//! its sealed receipts bind the prompt in that form. A request's tail is a trailing developer
//! message, after the final user item or tool output, which `OpenAI`'s implicit breakpoint skips.
//!
//! Only what the harness uses is encoded: the seven tools, the reasoning step, the output ceiling,
//! and the conversation. Sampling fields nobody reads, and the several Responses-only features the
//! harness has no place for, are absent for the same reason the chat path leaves them absent.

use nanus_domain::{Message, ToolSchema, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, ReasoningEffort};
use serde_json::{Map, Value, json};

use crate::Layout;
use crate::config::{CachePath, OpenAiConfig, Protocol, SystemCarrier, TailCarrier};

mod replay;
mod request;

pub use request::PreparedResponses;
pub(crate) use request::{estimate as estimate_request, prepare as prepare_request};

/// The most tool calls one response may carry.
const MAX_TOOL_CALLS: usize = 256;

/// Builds the JSON body for an ordinary Responses request.
#[must_use]
pub fn build_request(config: &OpenAiConfig, request: &ChatRequest) -> Value {
    encode(config, request, Layout::Ordinary)
}

/// Encodes a request: canonically, then decorated with its tail and cache hints.
///
/// The canonical body is the request with no tail and no breakpoint, its system prompt in the
/// path's carrier; decoration appends the tail and, when the runner asked for caching, places the
/// path's explicit breakpoints. What is sent, estimated and digested is the decorated body.
#[must_use]
pub fn encode(config: &OpenAiConfig, request: &ChatRequest, layout: Layout) -> Value {
    let path = config.cache_path(Protocol::Responses);
    let explicit = path.tail_carrier == TailCarrier::AppendedExplicit;
    let input = Value::Array(encode_items(&request.messages, false, explicit));
    let mut body = build_with_input(config, request, input, path.system_carrier);
    decorate(&mut body, request, layout, path);
    body
}

pub(crate) fn build_with_input(
    config: &OpenAiConfig,
    request: &ChatRequest,
    mut input: Value,
    system: SystemCarrier,
) -> Value {
    assert!(
        !request.messages.is_empty(),
        "a request carries at least one message"
    );
    let mut body = Map::new();
    // The request's id rather than the adapter's configured one: a runner switches models by naming
    // a different id in the request, without rebuilding the adapter. See the chat-completions path
    // for why the configured id is `LlmPort::model`'s answer and not the id every request sends.
    body.insert("model".to_owned(), json!(request.model));
    body.insert("stream".to_owned(), json!(true));
    // Nothing is kept server-side: the session log is the record, and a stored response would be a
    // second one that outlives the conversation it belongs to.
    body.insert("store".to_owned(), json!(false));
    match system {
        SystemCarrier::Developer => {
            if let (Some(item), Some(items)) =
                (developer_system(&request.messages), input.as_array_mut())
            {
                items.insert(0, item);
            }
        }
        SystemCarrier::Instructions | SystemCarrier::System => {
            if let Some(instructions) = instructions(&request.messages) {
                body.insert("instructions".to_owned(), json!(instructions));
            }
        }
    }
    body.insert("input".to_owned(), input);
    // The provider's ceiling, not the configured budget: a request above it is refused rather than
    // truncated, so sending it would fail every step. The `ChatGPT` backend — the endpoint whose own
    // shape this is — answers `Unsupported parameter: max_output_tokens`, so it is left to its own
    // ceiling there; the public API takes it.
    if config.sends_output_ceiling() {
        body.insert(
            "max_output_tokens".to_owned(),
            json!(
                request
                    .max_tokens
                    .unwrap_or_else(|| config.effective_max_tokens_for(&request.model))
            ),
        );
    }
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
        let mut tools = encode_tools(&request.tools);
        crate::function_policy::apply(config.function_strictness(), &mut tools, false);
        body.insert("tools".to_owned(), tools);
    }
    let encoded = Value::Object(body);
    assert!(encoded.get("model").is_some());
    assert!(encoded.get("input").is_some());
    encoded
}

/// The leading developer item that carries the system messages: one part each, in order.
///
/// `None` when there are none, so an absent prompt is absent rather than an empty message. An
/// empty system message has no part, as it would have added nothing to `instructions`.
fn developer_system(messages: &[Message]) -> Option<Value> {
    let parts: Vec<Value> = messages
        .iter()
        .filter_map(|message| match message {
            Message::System { text } if !text.is_empty() => {
                Some(json!({ "type": "input_text", "text": text }))
            }
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| json!({ "role": "developer", "content": parts }))
}

/// Places the tail after the newest item, in the form `carrier` names.
pub(crate) fn place_tail(input: &mut Vec<Value>, tail: &str, carrier: TailCarrier) {
    let part = json!({ "type": "input_text", "text": tail });
    match carrier {
        TailCarrier::TrailingDeveloper => {
            input.push(json!({ "role": "developer", "content": [part] }));
        }
        TailCarrier::Appended | TailCarrier::AppendedExplicit => {
            let last = input.last_mut();
            assert!(last.is_some(), "a tail follows an item");
            if let Some(last) = last {
                let parts = if last.get("output").is_some() {
                    &mut last["output"]
                } else {
                    &mut last["content"]
                };
                match parts {
                    Value::Array(parts) => parts.push(part),
                    Value::String(text) => {
                        text.push_str("\n\n");
                        text.push_str(tail);
                    }
                    other => *other = json!([part]),
                }
            }
        }
    }
}

/// The explicit breakpoint marker a part carries.
fn marker() -> Value {
    json!({ "mode": "explicit" })
}

/// Appends the tail and places the path's breakpoints.
pub(crate) fn decorate(body: &mut Value, request: &ChatRequest, layout: Layout, path: CachePath) {
    let Some(input) = body["input"].as_array_mut() else {
        return;
    };
    let before_tail = input.len();
    if let Some(tail) = request.context_tail.as_deref() {
        place_tail(input, tail, path.tail_carrier);
    }
    if request.prompt_cache == nanus_ports::PromptCacheRetention::None {
        return;
    }
    let explicit = path.tail_carrier == TailCarrier::AppendedExplicit;
    // The system prompt's own part, the first of the leading developer item, in both modes.
    if path.system_breakpoint
        && path.system_carrier == SystemCarrier::Developer
        && input
            .first()
            .is_some_and(|item| item["role"] == "developer")
        && let Some(first) = input[0]["content"].get_mut(0)
    {
        first["prompt_cache_breakpoint"] = marker();
    }
    let mut candidates: Vec<Option<(usize, usize)>> = Vec::new();
    // A tail appended as a part sits in the last item before decoration; the head's search must
    // stop short of it, or a first request's head entry would end in the tail and never be read.
    let appended =
        request.context_tail.is_some() && path.tail_carrier != TailCarrier::TrailingDeveloper;
    let tail_part = |at: usize| {
        (appended && at.saturating_add(1) == before_tail)
            .then(|| parts_of(&input[at]).map_or(0, Vec::len).saturating_sub(1))
    };
    if (path.head_breakpoint || explicit) && layout == Layout::Managed {
        candidates.push(head_item(input).and_then(|at| last_text_part(input, at, tail_part(at))));
    }
    if explicit {
        let answer = input
            .iter()
            .rposition(|item| item["role"] == "assistant" || item["type"] == "function_call");
        // The start of the last answer's group: its message and the calls that follow it.
        let answering =
            |item: &Value| item["role"] == "assistant" || item["type"] == "function_call";
        let group = answer.map(|at| {
            input[..at]
                .iter()
                .rposition(|item| !answering(item))
                .map_or(0, |before| before.saturating_add(1))
        });
        candidates.push(
            group
                .and_then(|at| at.checked_sub(1))
                .and_then(|at| last_part_at_or_before(input, at, None)),
        );
        let tailed = request.context_tail.is_some();
        let end = before_tail.checked_sub(1).map(|at| {
            let parts = parts_of(&input[at]).map_or(0, Vec::len);
            (
                at,
                if tailed {
                    parts.checked_sub(1)
                } else {
                    Some(parts)
                },
            )
        });
        candidates.push(end.and_then(|(at, upto)| last_part_at_or_before(input, at, upto)));
        body["prompt_cache_options"] = json!({ "mode": "explicit" });
    }
    let Some(input) = body["input"].as_array_mut() else {
        return;
    };
    let mut placed: Option<(usize, usize)> = None;
    for (item, part) in candidates.into_iter().flatten() {
        if placed.is_some_and(|last| (item, part) <= last) {
            continue;
        }
        let parts = if input[item].get("output").is_some() {
            &mut input[item]["output"]
        } else {
            &mut input[item]["content"]
        };
        if let Some(target) = parts.get_mut(part) {
            target["prompt_cache_breakpoint"] = marker();
            placed = Some((item, part));
        }
    }
}

/// The item that holds the head's last `input_text` part: the first user item.
///
/// The working-data message, when the head has one, is an `assistant` item whose part is
/// `output_text`, and a breakpoint is documented only on an `input_text` part. So the head's entry
/// ends at the first user message: the working data after it is re-read on every step, which costs
/// its few hundred bytes and never risks a refused request.
fn head_item(input: &[Value]) -> Option<usize> {
    input.iter().position(|item| item["role"] == "user")
}

/// The parts of an item that has them: a message's content, or an output given as a list.
fn parts_of(item: &Value) -> Option<&Vec<Value>> {
    item.get("output")
        .and_then(Value::as_array)
        .or_else(|| item["content"].as_array())
}

/// The last `input_text` part of the item at `at` (before `upto` within it).
fn last_text_part(input: &[Value], at: usize, upto: Option<usize>) -> Option<(usize, usize)> {
    let parts = parts_of(input.get(at)?)?;
    let parts = &parts[..upto.unwrap_or(parts.len()).min(parts.len())];
    let part = parts
        .iter()
        .rposition(|part| part["type"] == "input_text")?;
    Some((at, part))
}

/// The last part at or before item `at` (and before `upto` within it) that can carry a marker.
fn last_part_at_or_before(
    input: &[Value],
    at: usize,
    upto: Option<usize>,
) -> Option<(usize, usize)> {
    let mut limit = upto;
    for index in (0..=at).rev() {
        let count = parts_of(&input[index]).map_or(0, Vec::len);
        let end = limit.take().unwrap_or(count).min(count);
        if let Some(part) = end.checked_sub(1) {
            return Some((index, part));
        }
    }
    None
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
    Value::Array(encode_items(messages, false, false))
}

fn encode_replay_input(messages: &[Message]) -> Value {
    Value::Array(encode_items(messages, true, false))
}

/// Encodes the items; `explicit` gives every tool output as an `input_text` list, which the
/// [`TailCarrier::AppendedExplicit`] fallback needs for a part to mark or append to.
pub(crate) fn encode_items(messages: &[Message], original: bool, explicit: bool) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    // Pixels are not a tool output on this wire: a result that carries an image is answered with
    // a label, and the original ordered text and pixels follow as a user item once the whole group
    // of results has been answered, so every call is answered before anything else is said.
    let mut attachments: Vec<Value> = Vec::new();
    for message in messages {
        if !matches!(message, Message::Tool { .. }) {
            items.append(&mut attachments);
        }
        match message {
            // Lifted into the leading developer item or `instructions`; not repeated here.
            Message::System { .. } => {}
            Message::User {
                text,
                content_blocks,
            } => {
                let content = content_blocks.as_deref().map_or_else(
                    || vec![json!({"type":"input_text", "text":text})],
                    |blocks| blocks.iter().map(encode_input_block).collect(),
                );
                items.push(json!({"role":"user", "content":content}));
            }
            Message::Assistant {
                replay: Some(replay),
                ..
            } if original => {
                items.extend(replay.blocks.iter().cloned());
            }
            Message::Assistant {
                text, tool_calls, ..
            } => {
                encode_assistant(&mut items, text.as_deref(), tool_calls);
            }
            Message::Tool {
                call_id,
                content_blocks: Some(blocks),
                is_error,
                ..
            } if blocks
                .iter()
                .any(|block| matches!(block, nanus_domain::ContentBlock::Image { .. })) =>
            {
                encode_attachment(&mut items, &mut attachments, call_id, blocks, *is_error);
                if explicit && let Some(item) = items.last_mut() {
                    let label = item["output"].take();
                    item["output"] = output(label.as_str().unwrap_or_default().to_owned(), true);
                }
            }
            Message::Tool { call_id, .. } => {
                let mut item = json!({
                    "type": "function_call_output",
                    "call_id": call_id.as_str(),
                });
                // Assigned rather than written into the literal, which would copy the text again.
                item["output"] = output(message.tool_text().unwrap_or_default(), explicit);
                items.push(item);
            }
        }
    }
    items.append(&mut attachments);
    items
}

/// A tool output: its text, or a one-part `input_text` list under the explicit fallback. The text
/// is moved into the value, never copied.
fn output(text: String, explicit: bool) -> Value {
    if explicit {
        let mut part = Map::new();
        part.insert("type".to_owned(), json!("input_text"));
        part.insert("text".to_owned(), Value::String(text));
        Value::Array(vec![Value::Object(part)])
    } else {
        Value::String(text)
    }
}

fn encode_input_block(block: &nanus_domain::ContentBlock) -> Value {
    match block {
        nanus_domain::ContentBlock::Text(text) => json!({"type":"input_text", "text":text}),
        nanus_domain::ContentBlock::Image {
            media_type,
            data_base64,
        } => json!({
            "type":"input_image", "image_url":format!("data:{media_type};base64,{data_base64}"),
            "detail":"high"
        }),
    }
}

fn encode_assistant(items: &mut Vec<Value>, text: Option<&str>, calls: &[nanus_domain::ToolCall]) {
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        items.push(json!({"role":"assistant","content":[{"type":"output_text","text":text}]}));
    }
    for call in calls {
        items.push(json!({"type":"function_call","call_id":call.id.as_str(),
            "name":call.name.as_str(),"arguments":call.arguments.to_string()}));
    }
}

fn encode_attachment(
    items: &mut Vec<Value>,
    attachments: &mut Vec<Value>,
    call: &nanus_domain::ToolCallId,
    blocks: &[nanus_domain::ContentBlock],
    is_error: bool,
) {
    let label = nanus_ports::capabilities::attachment_label(call, is_error);
    items.push(json!({"type":"function_call_output","call_id":call.as_str(),"output":label}));
    let mut content = vec![json!({"type":"input_text","text":label})];
    content.extend(blocks.iter().map(|block| match block {
        nanus_domain::ContentBlock::Text(text) => json!({"type":"input_text","text":text}),
        nanus_domain::ContentBlock::Image {
            media_type,
            data_base64,
        } => json!({
            "type":"input_image","image_url":format!("data:{media_type};base64,{data_base64}"),
            "detail":"high"}),
    }));
    attachments.push(json!({"role":"user","content":content}));
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
    /// Set once the call is over its tool's raw limit. Its arguments are released at that moment,
    /// and nothing later restores them: a `done` event that carries the whole string cannot.
    oversized: bool,
}

/// Accumulates Responses events into the events the agent loop consumes.
///
/// The same shape as the chat accumulator, for the same reason: a function call's arguments arrive
/// as fragments keyed by an item, so the call cannot be reported until the stream ends.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    limits: Option<nanus_ports::ResponseLimits>,
    ready: Vec<LlmEvent>,
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    closed: bool,
    replay: Option<replay::Replay>,
    /// Raw per-tool argument limits; empty for an ordinary stream, which decodes unchanged.
    argument_limits: nanus_ports::ToolArgumentLimits,
}

impl StreamAccumulator {
    /// Retains completed stateless API items bound to the caller's already-admitted prefix.
    /// This decoder does not validate request provenance or authenticate provider ciphertext.
    /// # Errors
    /// Refuses a malformed digest. All replay observations require explicit response budgets.
    pub fn with_prefix(
        prefix_digest: String,
        limits: nanus_ports::ResponseLimits,
    ) -> nanus_ports::LlmResult<Self> {
        Ok(Self {
            limits: Some(limits),
            replay: Some(replay::Replay::new(prefix_digest, limits, None)?),
            ..Self::default()
        })
    }

    /// Retains original items with already-admitted source/wire fitting receipts.
    /// Receipt consistency and request provenance are the admitting caller's responsibility.
    /// # Errors
    /// Refuses malformed digests or impossible fitting receipt counts.
    pub fn with_context(
        prefix_digest: String,
        context: nanus_domain::message::ReplayContext,
        limits: nanus_ports::ResponseLimits,
    ) -> nanus_ports::LlmResult<Self> {
        Ok(Self {
            limits: Some(limits),
            replay: Some(replay::Replay::new(prefix_digest, limits, Some(context))?),
            ..Self::default()
        })
    }
    /// Sets response budgets before observation begins. Replay keeps its admitted budgets.
    pub fn set_response_limits(&mut self, limits: Option<nanus_ports::ResponseLimits>) {
        if self.replay.is_none() {
            self.limits = limits;
        }
    }

    /// Installs the raw argument limits a managed call is decoded under, before any frame.
    ///
    /// A call whose final name has a limit and whose buffered arguments pass it is reported with
    /// [`nanus_ports::OVERSIZED_ARGUMENTS`] instead of its arguments, so the oversized text never
    /// reaches a JSON parser. The same rule as the chat decoder: judged when the name is known,
    /// and bounded at one record before it is.
    pub fn set_argument_limits(&mut self, limits: nanus_ports::ToolArgumentLimits) {
        self.argument_limits = limits;
    }
    /// Responses has its own terminal payload and need not send a transport sentinel.
    #[must_use]
    pub const fn terminal_received(&self) -> bool {
        self.closed || self.finish.is_some()
    }
    /// Whether a terminal event has been queued.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Takes the next ready event, if any.
    pub fn take_ready(&mut self) -> Option<LlmEvent> {
        if self.ready.is_empty() {
            return None;
        }
        Some(self.ready.remove(0))
    }

    /// Records a failure as a terminal event.
    pub fn fail(&mut self, message: String) {
        if self.limits.is_some() {
            self.ready.clear();
            self.calls.clear();
            self.usage = None;
            self.finish = None;
            self.replay = None;
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
                crate::wire::truncate_for_message(payload)
            )));
            return;
        };
        self.observe_frame(&value);
    }

    /// Observes one decoded event.
    pub fn observe_frame(&mut self, frame: &Value) {
        if let Some(limits) = self.limits {
            if self.closed {
                return;
            }
            if let Err(error) = self.check_frame(frame, limits) {
                self.fail(error.to_string());
                return;
            }
        }
        if let Some(replay) = &mut self.replay
            && let Err(error) = replay.observe(frame)
        {
            self.fail(error.to_string());
            return;
        }
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
            "response.output_text.delta" | "response.refusal.delta"
                if kind == "response.output_text.delta" || self.replay.is_some() =>
            {
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
                ) && let Some(index) = self.calls.iter().position(|call| call.item_id == id)
                {
                    if let Some(call) = self.calls.get_mut(index)
                        && !call.oversized
                    {
                        call.arguments.push_str(delta);
                    }
                    self.judge_arguments(index);
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

    fn check_frame(
        &self,
        frame: &Value,
        limits: nanus_ports::ResponseLimits,
    ) -> nanus_ports::LlmResult<()> {
        limits.frame(frame)?;
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "response.function_call_arguments.delta" {
            let id = non_empty_str(frame, "item_id").unwrap_or_default();
            if let Some(call) = self.calls.iter().find(|call| call.item_id == id) {
                let added = frame
                    .get("delta")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                let arguments = nanus_ports::ResponseLimits::add(
                    "tool-call bytes",
                    call.arguments.len(),
                    added,
                    limits.event_bytes(),
                )?;
                limits.call_bytes(&call.call_id, &call.name, arguments)?;
            }
        }
        if matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) && let Some(item) = frame.get("item")
            && item.get("type").and_then(Value::as_str) == Some("function_call")
        {
            if kind == "response.output_item.added" && self.calls.len() >= limits.tool_slots() {
                return Err(nanus_ports::LlmError::ResponseLimit {
                    resource: "tool-call slots",
                    limit: limits.tool_slots(),
                });
            }
            let previous = if kind == "response.output_item.done" {
                let id = non_empty_str(item, "id").unwrap_or_default();
                self.calls.iter().find(|call| call.item_id == id)
            } else {
                None
            };
            let id = non_empty_str(item, "call_id")
                .or_else(|| previous.map(|call| call.call_id.as_str()))
                .unwrap_or_default();
            let name = non_empty_str(item, "name")
                .or_else(|| previous.map(|call| call.name.as_str()))
                .unwrap_or_default();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .map_or_else(|| previous.map_or(0, |call| call.arguments.len()), str::len);
            limits.call_bytes(id, name, arguments)?;
        }
        Ok(())
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
            oversized: false,
        });
        self.judge_arguments(self.calls.len().saturating_sub(1));
    }

    /// Judges one call against its tool's raw limit, and releases its arguments at once when it is
    /// over.
    ///
    /// Before the name is known there is no tool to judge by, so a managed stream bounds the
    /// unnamed buffer at one record: a call whose name never comes cannot hold more. Judged
    /// whenever the name is known, so a name that arrives after its arguments is checked against
    /// everything already buffered.
    fn judge_arguments(&mut self, index: usize) {
        if self.argument_limits.is_empty() {
            return;
        }
        let Some(call) = self.calls.get_mut(index) else {
            return;
        };
        if call.oversized {
            return;
        }
        let unnamed_over =
            call.name.is_empty() && call.arguments.len() > nanus_domain::content::RECORD_BYTES_MAX;
        let over = self
            .argument_limits
            .limit_for(&call.name)
            .is_some_and(|limit| call.arguments.len() > limit);
        if unnamed_over || over {
            call.oversized = true;
            call.arguments = String::new();
        }
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
        // A call already over its limit stays so: the whole string this event carries cannot
        // restore what the limit released.
        if !call.oversized
            && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
        {
            arguments.clone_into(&mut call.arguments);
        }
        if let Some(index) = self.calls.iter().position(|call| call.item_id == id) {
            self.judge_arguments(index);
        }
    }

    /// Emits the accumulated tool calls and the terminal event.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        if let Some(replay) = self.replay.take() {
            let checked = self
                .completed_calls()
                .and_then(|calls| replay.finish(&calls));
            match checked {
                Ok(replay) => self.ready.push(LlmEvent::AssistantReplay(replay)),
                Err(error) => {
                    self.fail(error.to_string());
                    return;
                }
            }
        }
        if self.limits.is_some()
            && self.calls.iter().any(|call| {
                call.call_id.is_empty() || nanus_domain::ToolName::new(call.name.clone()).is_err()
            })
        {
            self.fail("malformed stream: invalid completed tool call".into());
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
                arguments_delta: if call.oversized {
                    nanus_ports::OVERSIZED_ARGUMENTS.to_owned()
                } else {
                    call.arguments
                },
            });
        }
        if let Some(usage) = self.usage.take() {
            self.ready.push(LlmEvent::Usage(usage));
        }
        let reason = self.finish.take().unwrap_or(FinishReason::Stop);
        self.ready.push(LlmEvent::Finished { reason });
    }

    fn completed_calls(&self) -> nanus_ports::LlmResult<Vec<nanus_domain::ToolCall>> {
        let mut calls = Vec::with_capacity(self.calls.len());
        for call in &self.calls {
            let malformed = || nanus_ports::LlmError::Unsupported {
                feature: "malformed completed Responses function call".into(),
            };
            let arguments = serde_json::from_str(&call.arguments).map_err(|_| malformed())?;
            calls.push(nanus_domain::ToolCall {
                id: nanus_domain::ToolCallId::new(call.call_id.clone()),
                name: nanus_domain::ToolName::new(call.name.clone()).map_err(|_| malformed())?,
                arguments,
            });
        }
        Ok(calls)
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

    /// The `ChatGPT` backend refuses an output ceiling; the public API takes one.
    #[test]
    fn only_the_public_api_is_sent_an_output_ceiling() {
        let messages = vec![Message::user("hi")];
        let api = build_request(&config(), &request(messages.clone()));
        assert!(api["max_output_tokens"].as_u64().is_some(), "{api}");
        let mut backend = config();
        backend.set_protocol(Protocol::Responses);
        let body = build_request(&backend, &request(messages));
        assert!(body.get("max_output_tokens").is_none(), "{body}");
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

    /// A switched model reaches the Responses wire too: the request's id wins over the adapter's
    /// configured one, exactly as on the chat-completions path.
    #[test]
    fn the_request_names_the_model_it_carries_not_the_configured_one() {
        let configured = OpenAiConfig::new(Vendor::OpenAi, "gpt-5.3-codex", "token");
        let switched = ChatRequest::new("gpt-6-astra", vec![Message::user("hi")]);
        let body = build_request(&configured, &switched);
        assert_eq!(body["model"], json!("gpt-6-astra"), "{body}");
    }

    /// The prompt is a leading developer item, one part per system message, rather than
    /// `instructions`, and the turns become flat input items.
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
        assert!(body.get("instructions").is_none());
        assert_eq!(
            body["input"][0],
            json!({ "role": "developer", "content": [
                { "type": "input_text", "text": "be terse" },
                { "type": "input_text", "text": "and kind" },
            ] })
        );
        assert_eq!(body["input"][1]["role"], json!("user"));
        assert_eq!(body["input"][1]["content"][0]["type"], json!("input_text"));
        assert!(body.get("messages").is_none(), "the chat shape is not sent");
        assert_eq!(body["input"].as_array().map(Vec::len), Some(2));
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
                    replay: None,
                    text: None,
                    reasoning: None,
                    tool_calls: vec![call],
                },
                Message::Tool {
                    content_blocks: None,
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

#[cfg(test)]
#[path = "responses_bounds_tests.rs"]
mod bounds_tests;

#[cfg(test)]
mod argument_limit_tests {
    use nanus_domain::context::managed::limits::MANAGE_ARGUMENT_BYTES_MAX;
    use nanus_ports::{LlmEvent, OVERSIZED_ARGUMENTS, ToolArgumentLimits};
    use serde_json::{Value, json};

    use super::StreamAccumulator;

    /// The events a Responses stream decodes to, after its terminal event.
    fn decode(limits: Option<ToolArgumentLimits>, frames: &[Value]) -> Vec<LlmEvent> {
        let mut accumulator = StreamAccumulator::default();
        if let Some(limits) = limits {
            accumulator.set_argument_limits(limits);
        }
        for frame in frames {
            accumulator.observe_frame(frame);
        }
        accumulator.close();
        let mut events = Vec::new();
        while let Some(event) = accumulator.take_ready() {
            events.push(event);
        }
        events
    }

    fn arguments_of(events: &[LlmEvent]) -> Option<&str> {
        events.iter().find_map(|event| match event {
            LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } => Some(arguments_delta.as_str()),
            _ => None,
        })
    }

    /// A call whose name arrives only at `output_item.done`, with arguments over the context
    /// tool's limit, with the given `done` item.
    fn late_name_frames(arguments: &str, done_arguments: &str) -> Vec<Value> {
        vec![
            json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta",
                "item_id": "fc_1", "delta": arguments}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "context_manage", "arguments": done_arguments}}),
            json!({"type": "response.completed", "response": {}}),
        ]
    }

    #[test]
    fn a_managed_call_over_its_limit_is_cut_even_when_its_name_arrives_last() {
        let big = "x".repeat(MANAGE_ARGUMENT_BYTES_MAX + 1);
        let frames = late_name_frames(&big, &big);
        let events = decode(Some(ToolArgumentLimits::managed()), &frames);
        assert_eq!(arguments_of(&events), Some(OVERSIZED_ARGUMENTS));
    }

    #[test]
    fn a_done_event_carrying_the_whole_string_does_not_restore_an_oversized_call() {
        let big = "x".repeat(MANAGE_ARGUMENT_BYTES_MAX + 1);
        let frames = late_name_frames(&big, &big);
        let events = decode(Some(ToolArgumentLimits::managed()), &frames);
        assert_eq!(arguments_of(&events), Some(OVERSIZED_ARGUMENTS));
    }

    #[test]
    fn a_call_at_its_limit_is_kept_and_an_ordinary_stream_decodes_unchanged() {
        let at_limit = "x".repeat(MANAGE_ARGUMENT_BYTES_MAX);
        let frames = late_name_frames(&at_limit, &at_limit);
        let managed = decode(Some(ToolArgumentLimits::managed()), &frames);
        assert_eq!(arguments_of(&managed), Some(at_limit.as_str()));

        let over = "x".repeat(MANAGE_ARGUMENT_BYTES_MAX + 1);
        let frames = late_name_frames(&over, &over);
        let ordinary = decode(None, &frames);
        assert_eq!(arguments_of(&ordinary), Some(over.as_str()));
    }
}
