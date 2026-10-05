//! Explicit trusted instruction revisions; original receipts are never regenerated.
use super::{binding, digest, refused};
use crate::OpenAiConfig;
use nanus_domain::message::{ReplayContext, ReplayInstructions};
use nanus_ports::LlmResult;
use serde::{Serialize, Serializer, ser::SerializeMap as _};
use serde_json::Value;

#[derive(Serialize)]
struct Borrowed<'a> {
    version: u8,
    revision: &'a str,
    messages: &'a [&'a str],
}

pub(super) fn capture(
    config: &OpenAiConfig,
    source: &[nanus_domain::Message],
) -> LlmResult<Option<Box<ReplayInstructions>>> {
    if !config.instruction_revisions() {
        return Ok(None);
    }
    let mut messages = Vec::new();
    for message in source {
        let nanus_domain::Message::System { text } = message else {
            break;
        };
        if messages.len() == ReplayInstructions::MESSAGES_MAX {
            return Err(refused());
        }
        messages.push(text.as_str());
    }
    let revision = digest::json(&messages, ReplayInstructions::BYTES_MAX)?;
    let borrowed = Borrowed {
        version: 1,
        revision: &revision,
        messages: &messages,
    };
    nanus_domain::content::serialized_size(&borrowed, ReplayInstructions::BYTES_MAX)
        .map_err(|_| refused())?;
    let captured = ReplayInstructions {
        version: 1,
        revision,
        messages: messages.into_iter().map(str::to_owned).collect(),
    };
    captured.validate().map_err(|_| refused())?;
    Ok(Some(Box::new(captured)))
}

pub(super) fn context_budget(config: &OpenAiConfig, context: &ReplayContext) -> LlmResult<()> {
    if context.instructions.is_some() {
        let limit = config
            .response_limits()
            .ok_or_else(refused)?
            .event_bytes()
            .min(nanus_domain::content::RECORD_BYTES_MAX);
        nanus_domain::content::serialized_size(context, limit).map_err(|_| refused())?;
    }
    Ok(())
}

pub(super) fn original_binding(controls: &Value, context: &ReplayContext) -> LlmResult<String> {
    let Some(snapshot) = &context.instructions else {
        return binding(controls, context);
    };
    snapshot.validate().map_err(|_| refused())?;
    if snapshot.revision != digest::json(&snapshot.messages, ReplayInstructions::BYTES_MAX)? {
        return Err(refused());
    }
    let original = Historical {
        controls: controls.as_object().ok_or_else(refused)?,
        instructions: &snapshot.messages,
    };
    digest::json(
        &Binding {
            controls: original,
            context,
        },
        nanus_domain::content::RECORD_BYTES_MAX,
    )
}

// Borrow old snapshots and current unchanged controls while counting/hashing. This new mode
// has its own binding format; legacy Value-based canonical hashing remains byte-identical.
#[derive(Serialize)]
struct Binding<'a> {
    controls: Historical<'a>,
    context: &'a ReplayContext,
}
struct Historical<'a> {
    controls: &'a serde_json::Map<String, Value>,
    instructions: &'a [String],
}
impl Serialize for Historical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.controls.len()))?;
        for (key, value) in self.controls {
            if key == "instructions" {
                map.serialize_entry(key, self.instructions)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

pub(super) struct Continuity<'a> {
    current: Option<&'a ReplayInstructions>,
    previous: Option<&'a ReplayInstructions>,
    may_change: bool,
}
impl<'a> Continuity<'a> {
    pub(super) const fn new(current: Option<&'a ReplayInstructions>) -> Self {
        Self {
            current,
            previous: None,
            may_change: true,
        }
    }
    pub(super) const fn user(&mut self) {
        self.may_change = true;
    }
    pub(super) fn observe(&mut self, original: Option<&'a ReplayInstructions>) -> LlmResult<()> {
        if original.is_some() != self.current.is_some()
            || (!self.may_change && original != self.previous)
        {
            return Err(refused());
        }
        self.previous = original;
        self.may_change = false;
        Ok(())
    }
    pub(super) fn finish(&self) -> LlmResult<()> {
        if !self.may_change && self.current != self.previous {
            return Err(refused());
        }
        Ok(())
    }
}

pub(super) fn source_hash(revised: bool) -> LlmResult<digest::Source> {
    if revised {
        digest::Source::revised()
    } else {
        digest::Source::new()
    }
}
