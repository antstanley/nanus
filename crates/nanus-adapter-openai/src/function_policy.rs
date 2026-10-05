//! Caller-selected schema policy. Raw schema bytes/semantics stay under caller ownership.
use nanus_ports::{ChatRequest, LlmError, LlmResult};
use serde_json::Value;

use crate::{OpenAiConfig, Vendor};

const NODES_MAX: usize = 16_384;
// The API permits ten nesting levels; this traversal numbers the root at zero.
const DEPTH_MAX: usize = 9;

mod limits;

fn refused() -> LlmError {
    LlmError::Unsupported {
        feature: "incompatible explicit function-schema policy".into(),
    }
}

pub fn apply(strict: Option<bool>, tools: &mut Value, nested: bool) {
    let (Some(strict), Some(tools)) = (strict, tools.as_array_mut()) else {
        return;
    };
    for tool in tools {
        let function = if nested { &mut tool["function"] } else { tool };
        if let Some(object) = function.as_object_mut() {
            object.insert("strict".into(), Value::Bool(strict));
        }
    }
}

pub fn validate(config: &OpenAiConfig, request: &ChatRequest) -> LlmResult<()> {
    let Some(strict) = config.function_strictness() else {
        return Ok(());
    };
    if config.vendor() != Vendor::OpenAi {
        return Err(refused());
    }
    if strict {
        for tool in &request.tools {
            schema(&tool.parameters)?;
        }
    }
    Ok(())
}

/// A bounded, inlined subset: references/unknown schema keywords refuse instead of being guessed.
fn schema(root: &Value) -> LlmResult<()> {
    nanus_domain::content::serialized_size(root, nanus_domain::content::RECORD_BYTES_MAX)
        .map_err(|_| refused())?;
    if root["type"] != "object" || root.get("anyOf").is_some() {
        return Err(refused());
    }
    let mut pending = vec![(root, 0_usize)];
    let mut visited = 0_usize;
    let mut counts = limits::Counts::default();
    while let Some((value, depth)) = pending.pop() {
        visited = visited.checked_add(1).ok_or_else(refused)?;
        if visited > NODES_MAX || depth > DEPTH_MAX {
            return Err(refused());
        }
        let object = value.as_object().ok_or_else(refused)?;
        types(value)?;
        counts.observe(value)?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type"
                    | "description"
                    | "title"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "items"
                    | "anyOf"
                    | "enum"
            )
        }) {
            return Err(refused());
        }
        if value["type"] == "object"
            || value.get("properties").is_some()
            || value["type"]
                .as_array()
                .is_some_and(|types| types.iter().any(|v| v == "object"))
        {
            object_schema(value)?;
        }
        let children = children(value)?;
        if pending
            .len()
            .checked_add(children.len())
            .is_none_or(|count| count > NODES_MAX)
        {
            return Err(refused());
        }
        for child in children {
            pending.push((child, depth.saturating_add(1)));
        }
    }
    Ok(())
}

fn object_schema(value: &Value) -> LlmResult<()> {
    let properties = value["properties"].as_object().ok_or_else(refused)?;
    let required = value["required"].as_array().ok_or_else(refused)?;
    if properties.len() > NODES_MAX
        || required.len() > NODES_MAX
        || value["additionalProperties"] != false
        || properties.len() != required.len()
        || required.iter().any(|name| {
            name.as_str()
                .is_none_or(|name| !properties.contains_key(name))
        })
    {
        return Err(refused());
    }
    let mut names = std::collections::BTreeSet::new();
    for name in required {
        if !names.insert(name.as_str().ok_or_else(refused)?) {
            return Err(refused());
        }
    }
    Ok(())
}

fn children(value: &Value) -> LlmResult<Vec<&Value>> {
    let properties = match value.get("properties") {
        Some(value) => Some(value.as_object().ok_or_else(refused)?),
        None => None,
    };
    let alternatives = match value.get("anyOf") {
        Some(value) => Some(value.as_array().ok_or_else(refused)?),
        None => None,
    };
    let count = properties
        .map_or(0, serde_json::Map::len)
        .checked_add(alternatives.map_or(0, Vec::len))
        .and_then(|count| count.checked_add(usize::from(value.get("items").is_some())))
        .filter(|count| *count <= NODES_MAX)
        .ok_or_else(refused)?;
    let mut children = Vec::with_capacity(count);
    if let Some(properties) = properties {
        children.extend(properties.values());
    }
    if let Some(items) = value.get("items") {
        children.push(items);
    }
    if let Some(alternatives) = alternatives {
        if alternatives.is_empty() {
            return Err(refused());
        }
        children.extend(alternatives);
    }
    if children.len() > NODES_MAX {
        return Err(refused());
    }
    Ok(children)
}

fn known_type(value: &Value) -> bool {
    value.as_str().is_some_and(|name| {
        matches!(
            name,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        )
    })
}

fn includes_type(value: &Value, name: &str) -> bool {
    value["type"] == name
        || value["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|value| value == name))
}

fn types(value: &Value) -> LlmResult<()> {
    match value.get("type") {
        Some(Value::String(_)) if known_type(&value["type"]) => {}
        Some(Value::Array(types)) if !types.is_empty() && types.len() <= 7 => {
            let mut names = std::collections::BTreeSet::new();
            if types
                .iter()
                .any(|value| !known_type(value) || !names.insert(value.as_str()))
            {
                return Err(refused());
            }
        }
        None if value.get("anyOf").is_some() => {}
        _ => return Err(refused()),
    }
    if ["title", "description"]
        .iter()
        .any(|key| value.get(key).is_some_and(|value| !value.is_string()))
        || value
            .get("enum")
            .is_some_and(|value| value.as_array().is_none_or(Vec::is_empty))
        || (value.get("properties").is_some() && !includes_type(value, "object"))
        || (value.get("required").is_some() && !includes_type(value, "object"))
        || (value.get("additionalProperties").is_some() && !includes_type(value, "object"))
        || (includes_type(value, "array") && value.get("items").is_none())
        || (value.get("items").is_some() && !includes_type(value, "array"))
    {
        return Err(refused());
    }
    Ok(())
}
