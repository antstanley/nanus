//! The context tools: `context_manage` and `context_recall`, the pure half.
//!
//! Like the goal tools, these are offered beside the registered toolset and run by the loop
//! itself, because their subject is the current session — its accepted projection and its own
//! evidence — which a `'static` executor cannot reach. Unlike the goal tools they get no policy
//! bypass: each call is described to the host's [`nanus_ports::ToolPolicy`] with an explicit
//! access descriptor, goes through complete-batch admission, and is retained like any result.
//!
//! They are offered only in a session whose context is managed and whose selected model path
//! supports it, so a legacy session's schemas and count are exactly what they were.

use nanus_domain::context::managed::{MANAGE_TOOL, RECALL_TOOL};
use nanus_domain::{ToolAccess, ToolCall, ToolName, ToolSchema};
use serde_json::{Value, json};

/// Every context tool name, in the order they are offered.
pub const NAMES: [&str; 2] = [MANAGE_TOOL, RECALL_TOOL];

/// How many context tools there are.
pub const COUNT: usize = NAMES.len();

/// Whether `name` is a context tool.
#[must_use]
pub fn is_context_tool(name: &ToolName) -> bool {
    NAMES.contains(&name.as_str())
}

/// Whether a call asks to change the projection: a `context_manage` proposal.
///
/// Read from the raw arguments without parsing them into the input type, because the batch rule
/// that uses this runs before any argument is validated: a malformed proposal still counts as a
/// proposal, and a batch that mixes one with anything else is refused whole.
#[must_use]
pub fn is_mutating(call: &ToolCall) -> bool {
    call.name.as_str() == MANAGE_TOOL
        && call
            .arguments
            .get("action")
            .and_then(Value::as_str)
            .is_none_or(|action| action != "inspect")
}

/// The access descriptor a context call is shown to the host policy with.
///
/// Reads of the session's own state and evidence are `read`; a proposal writes the session's
/// projection, so it is `write` — never `execute`, because nothing runs and nothing outside the
/// session is touched.
#[must_use]
pub fn access(call: &ToolCall) -> ToolAccess {
    if is_mutating(call) {
        ToolAccess::Write
    } else {
        ToolAccess::Read
    }
}

/// The two schemas, in offer order.
#[must_use]
pub fn schemas() -> Vec<ToolSchema> {
    vec![manage_schema(), recall_schema()]
}

fn name(raw: &str) -> ToolName {
    ToolName::new(raw).unwrap_or_else(|_| unreachable!("a shipped tool name is valid"))
}

fn frontier() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["session_id", "event_count", "prefix_blake3", "projection_revision"],
        "properties": {
            "session_id": {"type": "string"},
            "event_count": {"type": "integer", "minimum": 0},
            "prefix_blake3": {"type": "string"},
            "projection_revision": {"type": "integer", "minimum": 0}
        }
    })
}

fn source() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "event_seq", "block_index", "artifact_id", "offset", "length",
            "source_digest", "field"],
        "properties": {
            "kind": {"enum": ["event", "artifact"]},
            "event_seq": {"type": ["integer", "null"]},
            "block_index": {"type": ["integer", "null"]},
            "artifact_id": {"type": ["string", "null"]},
            "offset": {"type": "integer", "minimum": 0},
            "length": {"type": "integer", "minimum": 0},
            "source_digest": {"type": "string"},
            "field": {"enum": ["user_text", "assistant_text", "assistant_reasoning", "tool_text",
                "tool_block", "artifact"]}
        }
    })
}

fn manage_schema() -> ToolSchema {
    let note = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "claim", "category", "sources"],
        "properties": {
            "id": {"type": "string", "description": "n: followed by 1-48 of [A-Za-z0-9_-]"},
            "claim": {"type": "string", "maxLength": 512},
            "category": {"enum": ["observed", "inferred", "unresolved", "superseded"]},
            "sources": {"type": "array", "minItems": 1, "maxItems": 4, "items": source()}
        }
    });
    ToolSchema {
        name: name(MANAGE_TOOL),
        description: "Inspect or change what earlier history this request shows. `inspect` \
            returns the accepted revision, frontier, profile digest, pressure and a page of \
            fragments (f:<seq>). `propose` hides or restores whole fragments and replaces all \
            working notes; echo base_revision, base_frontier and base_profile_digest from the \
            latest inspect. Every user message always stays visible. A proposal must be the \
            only call in its message and is applied only after this step settles."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action", "base_revision", "base_frontier", "hide", "restore", "notes",
                "cursor", "base_profile_digest"],
            "properties": {
                "action": {"enum": ["inspect", "propose"]},
                "base_revision": {"type": ["integer", "null"]},
                "base_frontier": {"anyOf": [frontier(), {"type": "null"}]},
                "hide": {"type": "array", "items": {"type": "string"}},
                "restore": {"type": "array", "items": {"type": "string"}},
                "notes": {"type": "array", "maxItems": 32, "items": note},
                "cursor": {"type": ["string", "null"]},
                "base_profile_digest": {"type": ["string", "null"]}
            }
        }),
    }
}

fn recall_schema() -> ToolSchema {
    ToolSchema {
        name: name(RECALL_TOOL),
        description: "Read this session's earlier evidence back, including hidden history and \
            archived shell output. `search` finds a literal, case-sensitive string; `read` \
            returns one bounded range of one source with its digest, for citing in notes."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action", "query", "target", "cursor", "limit", "max_bytes", "encoding"],
            "properties": {
                "action": {"enum": ["search", "read"]},
                "query": {"type": ["string", "null"]},
                "target": {"anyOf": [{
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "event_seq", "block_index", "artifact_id", "offset",
                        "length", "field"],
                    "properties": {
                        "kind": {"enum": ["event", "artifact"]},
                        "event_seq": {"type": ["integer", "null"]},
                        "block_index": {"type": ["integer", "null"]},
                        "artifact_id": {"type": ["string", "null"]},
                        "offset": {"type": "integer", "minimum": 0},
                        "length": {"type": "integer", "minimum": 0},
                        "field": {"enum": ["user_text", "assistant_text", "assistant_reasoning",
                            "tool_text", "tool_block", "artifact"]}
                    }
                }, {"type": "null"}]},
                "cursor": {"type": ["string", "null"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 40},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": 8192},
                "encoding": {"enum": ["text", "base64"]}
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::ToolCallId;

    fn call(tool: &str, arguments: Value) -> ToolCall {
        ToolCall::new(ToolCallId::new("c"), name(tool), arguments)
    }

    #[test]
    fn there_are_two_tools_and_only_their_allowed_fields_are_encoded() {
        let schemas = schemas();
        assert_eq!(schemas.len(), COUNT);
        for schema in &schemas {
            let encoded = serde_json::to_value(schema).unwrap_or_default();
            let keys: Vec<&String> = encoded
                .as_object()
                .map(|map| map.keys().collect())
                .unwrap_or_default();
            assert_eq!(keys.len(), 3, "{keys:?}");
            assert!(is_context_tool(&schema.name));
        }
        assert!(!is_context_tool(&name("read")));
    }

    #[test]
    fn only_a_proposal_mutates_and_a_malformed_manage_call_counts_as_one() {
        assert!(!is_mutating(&call(
            MANAGE_TOOL,
            json!({"action": "inspect"})
        )));
        assert!(is_mutating(&call(
            MANAGE_TOOL,
            json!({"action": "propose"})
        )));
        assert!(is_mutating(&call(MANAGE_TOOL, json!("not an object"))));
        assert!(!is_mutating(&call(
            RECALL_TOOL,
            json!({"action": "propose"})
        )));
        assert_eq!(
            access(&call(MANAGE_TOOL, json!({"action": "propose"}))),
            ToolAccess::Write
        );
        assert_eq!(access(&call(RECALL_TOOL, json!({}))), ToolAccess::Read);
    }
}
