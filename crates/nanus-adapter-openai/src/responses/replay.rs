//! Completed original items, bounded before copies. Ciphertext is opaque, never authenticated here.
use std::collections::BTreeMap;

use nanus_domain::message::{AssistantReplay, ReplayContext};
use nanus_ports::{LlmError, LlmResult, ResponseLimits};
use serde_json::Value;

const PARTS_MAX: usize = 256;

mod capacity;

#[derive(Debug)]
struct Slot {
    id: String,
    kind: String,
    call_id: Option<String>,
    name: Option<String>,
    phase: Option<String>,
    arguments: String,
    parts: BTreeMap<(String, usize), String>,
    done: Option<Value>,
}

#[derive(Debug)]
pub struct Replay {
    prefix: String,
    limits: ResponseLimits,
    bytes: usize,
    frames: usize,
    slots: BTreeMap<usize, Slot>,
    response_id: Option<String>,
    completed: bool,
    text: String,
    context: Option<ReplayContext>,
}

fn refused() -> LlmError {
    LlmError::Unsupported {
        feature: "malformed or incomplete stateless Responses replay".into(),
    }
}

fn string<'a>(value: &'a Value, name: &str) -> LlmResult<&'a str> {
    value[name].as_str().ok_or_else(refused)
}

impl Replay {
    pub fn new(
        prefix: String,
        limits: ResponseLimits,
        context: Option<ReplayContext>,
    ) -> LlmResult<Self> {
        if prefix.len() != 64 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(refused());
        }
        if let Some(context) = &context {
            context.validate().map_err(|_| refused())?;
        }
        Ok(Self {
            prefix,
            limits,
            bytes: 0,
            frames: 0,
            slots: BTreeMap::new(),
            response_id: None,
            completed: false,
            text: String::new(),
            context,
        })
    }

    pub fn observe(&mut self, frame: &Value) -> LlmResult<()> {
        if self.completed {
            return Err(refused());
        }
        // Charge every payload, including unknown metadata, before retaining any part of it.
        self.limits.frame(frame)?;
        let bytes = nanus_domain::content::serialized_size(frame, self.limits.response_bytes())
            .map_err(|_| refused())?;
        self.bytes = ResponseLimits::add(
            "replay response bytes",
            self.bytes,
            bytes,
            self.limits.response_bytes(),
        )?;
        self.frames = ResponseLimits::add("replay frames", self.frames, 1, self.limits.events())?;
        match string(frame, "type")? {
            "response.created" | "response.in_progress" => self.header(&frame["response"]),
            "response.output_item.added" => self.added(frame),
            "response.output_item.done" => self.done(frame),
            "response.function_call_arguments.delta" => self.arguments(frame),
            "response.function_call_arguments.done" => self.arguments_done(frame),
            "response.output_text.delta" => self.part(frame, "output_text", "content_index"),
            "response.refusal.delta" => self.part(frame, "refusal", "content_index"),
            "response.reasoning_summary_text.delta" => {
                self.part(frame, "summary_text", "summary_index")
            }
            "response.reasoning_text.delta" => self.part(frame, "reasoning_text", "content_index"),
            "response.output_text.done" => {
                self.part_done(frame, "output_text", "content_index", "text")
            }
            "response.refusal.done" => self.part_done(frame, "refusal", "content_index", "refusal"),
            "response.reasoning_summary_text.done" => {
                self.part_done(frame, "summary_text", "summary_index", "text")
            }
            "response.reasoning_text.done" => {
                self.part_done(frame, "reasoning_text", "content_index", "text")
            }
            "response.completed" => self.complete(&frame["response"]),
            "response.incomplete" | "response.failed" | "error" => Err(refused()),
            _ => self.metadata(frame),
        }
    }

    fn header(&mut self, response: &Value) -> LlmResult<()> {
        let id = string(response, "id")?;
        if id.is_empty() || response["status"] != "in_progress" {
            return Err(refused());
        }
        match &self.response_id {
            Some(previous) if previous != id => Err(refused()),
            Some(_) => Ok(()),
            None => {
                self.response_id = Some(id.to_owned());
                Ok(())
            }
        }
    }

    fn index(&self, frame: &Value) -> LlmResult<usize> {
        self.limits
            .index(frame["output_index"].as_u64().ok_or_else(refused)?)
    }

    fn added(&mut self, frame: &Value) -> LlmResult<()> {
        let index = self.index(frame)?;
        let item = &frame["item"];
        let id = string(item, "id")?;
        let kind = string(item, "type")?;
        Self::added_shape(item, kind)?;
        if id.is_empty()
            || self.slots.contains_key(&index)
            || self.slots.values().any(|slot| slot.id == id)
            || !matches!(kind, "reasoning" | "message" | "function_call")
            || item
                .get("status")
                .is_some_and(|v| !v.is_null() && v != "in_progress")
        {
            return Err(refused());
        }
        let (call_id, name, arguments) = if kind == "function_call" {
            let call_id = string(item, "call_id")?;
            let name = string(item, "name")?;
            let arguments = string(item, "arguments")?;
            if call_id.is_empty()
                || nanus_domain::ToolName::new(name.to_owned()).is_err()
                || self
                    .slots
                    .values()
                    .any(|slot| slot.call_id.as_deref() == Some(call_id))
            {
                return Err(refused());
            }
            self.limits.call_bytes(call_id, name, arguments.len())?;
            (
                Some(call_id.to_owned()),
                Some(name.to_owned()),
                arguments.to_owned(),
            )
        } else {
            (None, None, String::new())
        };
        self.slots.insert(
            index,
            Slot {
                id: id.to_owned(),
                kind: kind.to_owned(),
                call_id,
                name,
                phase: item["phase"].as_str().map(str::to_owned),
                arguments,
                parts: BTreeMap::new(),
                done: None,
            },
        );
        Ok(())
    }

    fn slot_mut(&mut self, frame: &Value) -> LlmResult<&mut Slot> {
        let index = self.index(frame)?;
        let id = string(frame, "item_id")?;
        let slot = self.slots.get_mut(&index).ok_or_else(refused)?;
        if slot.id != id || slot.done.is_some() {
            return Err(refused());
        }
        Ok(slot)
    }

    fn added_shape(item: &Value, kind: &str) -> LlmResult<()> {
        let fields: &[&str] = match kind {
            "reasoning" => &[
                "type",
                "id",
                "summary",
                "content",
                "encrypted_content",
                "status",
            ],
            "message" => &["type", "id", "role", "status", "content", "phase"],
            "function_call" => &["type", "id", "call_id", "name", "arguments", "status"],
            _ => return Err(refused()),
        };
        let object = item.as_object().ok_or_else(refused)?;
        if object.keys().any(|key| !fields.contains(&key.as_str())) {
            return Err(refused());
        }
        if kind == "message"
            && (item["role"] != "assistant"
                || item["content"].as_array().is_none_or(|v| !v.is_empty())
                || item.get("phase").is_some_and(|v| {
                    !v.is_null() && !matches!(v.as_str(), Some("commentary" | "final_answer"))
                }))
        {
            return Err(refused());
        }
        if kind == "reasoning"
            && (item["summary"].as_array().is_none_or(|v| !v.is_empty())
                || item
                    .get("content")
                    .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
                || item
                    .get("encrypted_content")
                    .is_some_and(|v| !v.is_null() && !v.is_string()))
        {
            return Err(refused());
        }
        Ok(())
    }

    fn arguments(&mut self, frame: &Value) -> LlmResult<()> {
        let delta = string(frame, "delta")?;
        let limits = self.limits;
        let slot = self.slot_mut(frame)?;
        if slot.kind != "function_call" {
            return Err(refused());
        }
        let bytes = ResponseLimits::add(
            "tool-call bytes",
            slot.arguments.len(),
            delta.len(),
            limits.event_bytes(),
        )?;
        limits.call_bytes(
            slot.call_id.as_deref().ok_or_else(refused)?,
            slot.name.as_deref().ok_or_else(refused)?,
            bytes,
        )?;
        slot.arguments.push_str(delta);
        Ok(())
    }

    fn arguments_done(&mut self, frame: &Value) -> LlmResult<()> {
        let slot = self.slot_mut(frame)?;
        if slot.kind != "function_call" || frame["arguments"] != slot.arguments {
            return Err(refused());
        }
        Ok(())
    }

    fn part_done(
        &mut self,
        frame: &Value,
        kind: &str,
        index_key: &str,
        text_key: &str,
    ) -> LlmResult<()> {
        let index = frame[index_key]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v < PARTS_MAX)
            .ok_or_else(refused)?;
        let text = string(frame, text_key)?;
        let slot = self.slot_mut(frame)?;
        let message = matches!(kind, "output_text" | "refusal");
        if slot.kind != if message { "message" } else { "reasoning" } {
            return Err(refused());
        }
        if slot
            .parts
            .get(&(kind.to_owned(), index))
            .map_or("", String::as_str)
            != text
        {
            return Err(refused());
        }
        Ok(())
    }

    fn part(&mut self, frame: &Value, kind: &str, index_key: &str) -> LlmResult<()> {
        let delta = string(frame, "delta")?;
        let index = frame[index_key]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v < PARTS_MAX)
            .ok_or_else(refused)?;
        let limits = self.limits;
        let slot = self.slot_mut(frame)?;
        let message = matches!(kind, "output_text" | "refusal");
        if slot.kind != if message { "message" } else { "reasoning" } {
            return Err(refused());
        }
        let key = (kind.to_owned(), index);
        if !slot.parts.contains_key(&key) && slot.parts.len() >= PARTS_MAX {
            return Err(refused());
        }
        let used = slot.parts.get(&key).map_or(0, String::len);
        ResponseLimits::add("replay part bytes", used, delta.len(), limits.event_bytes())?;
        slot.parts.entry(key).or_default().push_str(delta);
        if message {
            ResponseLimits::add(
                "replay text bytes",
                self.text.len(),
                delta.len(),
                limits.event_bytes(),
            )?;
            self.text.push_str(delta);
        }
        Ok(())
    }

    fn done(&mut self, frame: &Value) -> LlmResult<()> {
        let index = self.index(frame)?;
        let item = &frame["item"];
        let slot = self.slots.get(&index).ok_or_else(refused)?;
        if slot.done.is_some() || item["id"] != slot.id || item["type"] != slot.kind {
            return Err(refused());
        }
        AssistantReplay::validate_item("openai.responses", item).map_err(|_| refused())?;
        if slot
            .phase
            .as_deref()
            .is_some_and(|phase| item["phase"] != phase)
        {
            return Err(refused());
        }
        if slot.kind == "function_call" {
            if item["call_id"].as_str() != slot.call_id.as_deref()
                || item["name"].as_str() != slot.name.as_deref()
                || item["arguments"] != slot.arguments
            {
                return Err(refused());
            }
        } else {
            Self::parts_agree(slot, item)?;
        }
        // Include the complete envelope and escaping before retaining the original item.
        capacity::check(
            &self.prefix,
            self.context.as_ref(),
            &self.slots,
            index,
            item,
            self.limits.event_bytes(),
        )?;
        self.slots.get_mut(&index).ok_or_else(refused)?.done = Some(item.clone());
        Ok(())
    }

    fn parts_agree(slot: &Slot, item: &Value) -> LlmResult<()> {
        let mut expected = BTreeMap::new();
        for field in ["summary", "content"] {
            if let Some(parts) = item[field].as_array() {
                for (index, part) in parts.iter().enumerate() {
                    let kind = string(part, "type")?;
                    let text = string(part, if kind == "refusal" { "refusal" } else { "text" })?;
                    expected.insert((kind.to_owned(), index), text);
                }
            }
        }
        for (key, text) in &expected {
            if slot.parts.get(key).map_or("", String::as_str) != *text {
                return Err(refused());
            }
        }
        if slot.parts.keys().any(|key| !expected.contains_key(key)) {
            return Err(refused());
        }
        Ok(())
    }

    fn complete(&mut self, response: &Value) -> LlmResult<()> {
        if response["status"] != "completed"
            || response.get("error").is_some_and(|v| !v.is_null())
            || response
                .get("incomplete_details")
                .is_some_and(|v| !v.is_null())
            || response["id"].as_str() != self.response_id.as_deref()
        {
            return Err(refused());
        }
        Self::usage(&response["usage"])?;
        let items = response["output"].as_array().ok_or_else(refused)?;
        if items.is_empty() || items.len() != self.slots.len() {
            return Err(refused());
        }
        for (index, item) in items.iter().enumerate() {
            if self.slots.get(&index).and_then(|slot| slot.done.as_ref()) != Some(item) {
                return Err(refused());
            }
        }
        self.completed = true;
        Ok(())
    }

    fn usage(value: &Value) -> LlmResult<()> {
        if value.is_null() {
            return Ok(());
        }
        let count = |v: &Value| {
            v.as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(refused)
        };
        let input = count(&value["input_tokens"])?;
        let output = count(&value["output_tokens"])?;
        let total = count(&value["total_tokens"])?;
        if input.checked_add(output) != Some(total) {
            return Err(refused());
        }
        for (parent, key, ceiling) in [
            ("input_tokens_details", "cached_tokens", input),
            ("output_tokens_details", "reasoning_tokens", output),
        ] {
            if let Some(details) = value.get(parent).filter(|v| !v.is_null())
                && count(&details[key])? > ceiling
            {
                return Err(refused());
            }
        }
        Ok(())
    }

    fn metadata(&self, frame: &Value) -> LlmResult<()> {
        // Metadata may annotate a live item, but it may not point to an absent or crossed item.
        if frame.get("item_id").is_some() || frame.get("output_index").is_some() {
            let index = self.index(frame)?;
            let slot = self.slots.get(&index).ok_or_else(refused)?;
            if frame["item_id"] != slot.id {
                return Err(refused());
            }
        }
        Ok(())
    }

    pub fn finish(self, calls: &[nanus_domain::ToolCall]) -> LlmResult<AssistantReplay> {
        if !self.completed {
            return Err(refused());
        }
        let blocks = self
            .slots
            .into_values()
            .map(|slot| slot.done.ok_or_else(refused))
            .collect::<LlmResult<Vec<_>>>()?;
        let replay = AssistantReplay {
            protocol: "openai.responses".into(),
            prefix_digest: self.prefix,
            context_receipt: self.context.map(Box::new),
            blocks,
        };
        replay
            .validate_response(Some(&self.text), calls)
            .map_err(|_| refused())?;
        nanus_domain::content::serialized_size(&replay, self.limits.event_bytes()).map_err(
            |_| LlmError::ResponseLimit {
                resource: "assistant replay bytes",
                limit: self.limits.event_bytes(),
            },
        )?;
        Ok(replay)
    }
}
