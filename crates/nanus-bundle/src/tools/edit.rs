//! The `edit` tool: replace text in a file.
//!
//! The contract is deliberately strict. By default `old_string` must occur
//! **exactly once**: zero occurrences means the model's mental model of the file is
//! stale, and more than one means the edit is ambiguous and a silent first-match
//! replacement would change a line the model never looked at. Both are reported, and
//! `replace_all` is the explicit way to ask for every occurrence.
//!
//! That single rule is what makes an edit reviewable. An edit that can match
//! anywhere is an edit whose effect cannot be predicted from its arguments.

use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::FsHandle;
use serde_json::json;

use crate::args::Arguments;
use crate::tools::port_error_result;

/// Builds the `edit` tool over `fs`.
pub fn edit_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("edit").unwrap_or_else(|_| unreachable!("edit is a valid tool name")),
        description: "Replace text in a file. By default `old_string` must appear exactly once \
                      in the file, which makes the edit unambiguous; set `replace_all` to \
                      replace every occurrence. Read the file first so the text you match is \
                      what is actually there."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace root."
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to put in its place. An empty string deletes it."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence rather than requiring exactly one. \
                                    Defaults to false."
                }
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, EditExecutor { fs }).with_access(ToolAccess::Write)
}

/// Executes `edit`.
struct EditExecutor {
    fs: FsHandle,
}

impl ToolExecutor for EditExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move { edit_outcome(fs, call).await })
    }
}

/// Performs the edit and renders the outcome.
async fn edit_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("edit", &call.arguments);
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let old = match arguments.required_str("old_string") {
        Ok(old) => old,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let new = match arguments.required_str("new_string") {
        Ok(new) => new,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let replace_all = arguments.flag("replace_all").unwrap_or(false);

    if old.is_empty() {
        // Replacing the empty string would insert at every position, which no model
        // means by it.
        return ToolResult::new(
            id,
            ToolOutcome::failure("edit: old_string is empty; pass the text to replace"),
        );
    }
    if old == new {
        // A no-op edit usually means the model has lost track of the file's contents.
        return ToolResult::new(
            id,
            ToolOutcome::failure(
                "edit: old_string and new_string are identical, so nothing changes",
            ),
        );
    }

    let edited = fs
        .edit(std::path::Path::new(&path), &old, &new, replace_all)
        .await;
    let outcome = match edited {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "edit", &error),
    };

    let diff = outcome.unified_diff();
    let value = json!({
        "file_path": path,
        "replacements": outcome.replacements,
        "bytes_before": outcome.before.len(),
        "bytes_after": outcome.after.len(),
    });
    // The diff is the model-visible content: it shows what changed, which is the
    // thing the model must verify before its next step.
    let text = format!(
        "Edited {path} ({} replacement{}).\n{diff}",
        outcome.replacements,
        if outcome.replacements == 1 { "" } else { "s" }
    );
    ToolResult::new(
        id,
        ToolOutcome::success_with(value, vec![ContentBlock::Text(text)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::ToolCallId;
    use nanus_ports::FsPort;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            ToolCallId::new("call-e"),
            ToolName::new("edit").unwrap_or_else(|_| unreachable!("edit is valid")),
            arguments,
        )
    }

    /// A filesystem handle for schema-only assertions.
    fn fs_handle() -> FsHandle {
        let port: Box<dyn FsPort> = Box::new(crate::tests_support::UnusedFs);
        std::rc::Rc::new(port)
    }

    #[test]
    fn the_schema_requires_the_three_essential_fields() {
        let definition = edit_tool(fs_handle());
        let schema = definition.schema().parameters.to_string();
        assert!(schema.contains("old_string"), "{schema}");
        assert!(schema.contains("new_string"), "{schema}");
        assert!(schema.contains("replace_all"), "{schema}");
        // `replace_all` must default rather than be required: the common case is a
        // unique edit.
        assert!(
            schema.contains("\"required\":[\"file_path\",\"old_string\",\"new_string\"]"),
            "{schema}"
        );
    }

    #[tokio::test]
    async fn an_empty_old_string_is_refused_before_touching_the_file() {
        let definition = edit_tool(fs_handle());
        let result = definition
            .execute(call(
                json!({ "file_path": "a.txt", "old_string": "", "new_string": "x" }),
            ))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("an empty pattern is a failure");
        };
        assert!(message.contains("empty"), "{message}");
    }

    #[tokio::test]
    async fn an_identical_replacement_is_refused() {
        let definition = edit_tool(fs_handle());
        let result = definition
            .execute(call(json!({
                "file_path": "a.txt",
                "old_string": "same",
                "new_string": "same"
            })))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("a no-op edit is a failure");
        };
        assert!(message.contains("identical"), "{message}");
    }
}
