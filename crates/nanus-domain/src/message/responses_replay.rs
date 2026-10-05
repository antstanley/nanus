//! The bounded stateless subset: original reasoning, assistant messages and ordinary functions.
use crate::{
    ToolCall, ToolName,
    content::{ContentError, RECORD_BYTES_MAX, serialized_size},
};
use serde_json::Value;

mod annotations;

fn invalid() -> ContentError {
    ContentError::new("invalid Responses replay item")
}

fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<(), ContentError> {
    let object = value.as_object().ok_or_else(invalid)?;
    if required.iter().any(|name| !object.contains_key(*name))
        || object
            .keys()
            .any(|name| !required.contains(&name.as_str()) && !optional.contains(&name.as_str()))
    {
        return Err(invalid());
    }
    Ok(())
}
fn string(value: &Value, field: &str) -> bool {
    value[field].is_string()
}
fn present(value: &Value, field: &str) -> bool {
    value[field].as_str().is_some_and(|value| !value.is_empty())
}
fn completed(value: &Value) -> bool {
    value
        .get("status")
        .is_none_or(|status| status.is_null() || status == "completed")
}
fn list(value: &Value) -> Result<&Vec<Value>, ContentError> {
    value
        .as_array()
        .filter(|items| items.len() <= 256)
        .ok_or_else(invalid)
}

pub(super) fn item(value: &Value) -> Result<(), ContentError> {
    serialized_size(value, RECORD_BYTES_MAX)?;
    if !present(value, "id") || !completed(value) {
        return Err(invalid());
    }
    match value["type"].as_str() {
        Some("reasoning") => reasoning(value),
        Some("message") => message(value),
        Some("function_call") => function(value),
        _ => Err(invalid()),
    }
}

fn reasoning(value: &Value) -> Result<(), ContentError> {
    fields(
        value,
        &["id", "type", "summary", "encrypted_content"],
        &["status", "content"],
    )?;
    if !present(value, "encrypted_content") {
        return Err(invalid());
    }
    for summary in list(&value["summary"])? {
        fields(summary, &["type", "text"], &[])?;
        if summary["type"] != "summary_text" || !string(summary, "text") {
            return Err(invalid());
        }
    }
    if let Some(content) = value.get("content").filter(|content| !content.is_null()) {
        for text in list(content)? {
            fields(text, &["type", "text"], &[])?;
            if text["type"] != "reasoning_text" || !string(text, "text") {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn message(value: &Value) -> Result<(), ContentError> {
    fields(
        value,
        &["id", "type", "role", "status", "content"],
        &["phase"],
    )?;
    if value["role"] != "assistant"
        || value["status"] != "completed"
        || value.get("phase").is_some_and(|phase| {
            !phase.is_null() && !matches!(phase.as_str(), Some("commentary" | "final_answer"))
        })
    {
        return Err(invalid());
    }
    for content in list(&value["content"])? {
        match content["type"].as_str() {
            Some("output_text") => output_text(content)?,
            Some("refusal") => {
                fields(content, &["type", "refusal"], &[])?;
                if !string(content, "refusal") {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

fn output_text(value: &Value) -> Result<(), ContentError> {
    fields(value, &["type", "text", "annotations"], &["logprobs"])?;
    if !string(value, "text")
        || value.get("logprobs").is_some_and(|value| {
            !value.is_null() && value.as_array().is_none_or(|items| !items.is_empty())
        })
    {
        return Err(invalid());
    }
    for annotation in list(&value["annotations"])? {
        annotations::validate(annotation)?;
    }
    Ok(())
}

fn function(value: &Value) -> Result<(), ContentError> {
    fields(
        value,
        &["id", "type", "call_id", "name", "arguments"],
        &["status"],
    )?;
    if !present(value, "call_id")
        || value["name"]
            .as_str()
            .is_none_or(|name| ToolName::new(name).is_err())
    {
        return Err(invalid());
    }
    arguments(value).map(|_| ())
}
fn arguments(value: &Value) -> Result<Value, ContentError> {
    let raw = value["arguments"].as_str().ok_or_else(invalid)?;
    let parsed: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    if !parsed.is_object() {
        return Err(invalid());
    }
    Ok(parsed)
}

pub(super) fn identities(blocks: &[Value]) -> Result<(), ContentError> {
    let mut items = std::collections::BTreeSet::new();
    let mut calls = std::collections::BTreeSet::new();
    for block in blocks {
        if !items.insert(block["id"].as_str().ok_or_else(invalid)?)
            || (block["type"] == "function_call"
                && !calls.insert(block["call_id"].as_str().ok_or_else(invalid)?))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn response(
    blocks: &[Value],
    text: Option<&str>,
    calls: &[ToolCall],
) -> Result<(), ContentError> {
    let mut original = String::new();
    let mut index = 0_usize;
    for block in blocks {
        if block["type"] == "message" {
            for part in list(&block["content"])? {
                let field = if part["type"] == "refusal" {
                    "refusal"
                } else {
                    "text"
                };
                original.push_str(part[field].as_str().ok_or_else(invalid)?);
            }
        } else if block["type"] == "function_call" {
            let call = calls.get(index).ok_or_else(invalid)?;
            if block["call_id"] != call.id.as_str()
                || block["name"] != call.name.as_str()
                || arguments(block)? != call.arguments
            {
                return Err(invalid());
            }
            index = index.checked_add(1).ok_or_else(invalid)?;
        }
    }
    if original != text.unwrap_or_default() || index != calls.len() {
        return Err(invalid());
    }
    Ok(())
}
