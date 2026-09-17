//! The `write` tool.
//!
//! Writing replaces a file wholesale, which is the right shape for creating
//! something new and a destructive shape for changing something existing. The tool
//! therefore distinguishes the two in its arguments (`mode`), in its result, and in
//! the description the model reads — rather than deciding on the model's behalf.
//!
//! Editing an existing file is what [`crate::tools::edit_tool`] is for, and the
//! description says so, because a model that rewrites a file to change one line
//! loses everything it did not think to reproduce.

use nanus_domain::{
    ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolOutcome,
    ToolResult, ToolSchema,
};
use nanus_ports::{FsHandle, WriteMode};
use serde_json::json;

use crate::args::Arguments;
use crate::tools::{port_error_result, text_success};

/// Builds the `write` tool over `fs`.
pub fn write_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("write").unwrap_or_else(|_| unreachable!("write is a valid tool name")),
        description: "Write a file. Use `mode: \"create\"` to create a new file (it fails if \
                      the file already exists) and `mode: \"overwrite\"` to replace one \
                      entirely. To change part of an existing file, use `edit` instead: \
                      overwriting loses everything you do not reproduce."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace root."
                },
                "content": {
                    "type": "string",
                    "description": "The complete contents to write."
                },
                "mode": {
                    "type": "string",
                    "enum": ["create", "overwrite"],
                    "description": "Whether the file must not exist, or may be replaced."
                }
            },
            "required": ["file_path", "content", "mode"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, WriteExecutor { fs }).with_access(ToolAccess::Write)
}

/// Executes `write`.
struct WriteExecutor {
    fs: FsHandle,
}

impl ToolExecutor for WriteExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move { write_outcome(fs, call).await })
    }
}

/// Writes a file and renders the outcome.
async fn write_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("write", &call.arguments);
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let content = match arguments.required_str("content") {
        Ok(content) => content,
        Err(failure) => return ToolResult::new(id, failure),
    };
    // `mode` is required rather than defaulted: silently overwriting is the one
    // mistake this tool must not make on the model's behalf.
    let raw_mode = match arguments.required_str("mode") {
        Ok(mode) => mode,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let mode = match raw_mode.as_str() {
        "create" => WriteMode::Create,
        "overwrite" => WriteMode::Overwrite,
        other => {
            return ToolResult::new(
                id,
                ToolOutcome::failure(format!(
                    "write: mode must be \"create\" or \"overwrite\", but it was {other:?}"
                )),
            );
        }
    };

    let written = fs.write(std::path::Path::new(&path), &content, mode).await;
    let outcome = match written {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "write", &error),
    };

    let verb = if outcome.created { "Created" } else { "Wrote" };
    let value = json!({
        "file_path": path,
        "bytes_written": outcome.bytes_written,
        "created": outcome.created,
    });
    let text = format!("{verb} {path} ({} bytes).", outcome.bytes_written);
    ToolResult::new(id, text_success(value, text))
}

#[cfg(test)]
mod tests {
    use nanus_ports::FsPort;

    use nanus_domain::ToolCallId;

    use super::*;

    fn fs_handle() -> FsHandle {
        let port: Box<dyn FsPort> = Box::new(crate::tests_support::UnusedFs);
        std::rc::Rc::new(port)
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            ToolCallId::new("call-w"),
            ToolName::new("write").unwrap_or_else(|_| unreachable!("write is valid")),
            arguments,
        )
    }

    #[tokio::test]
    async fn an_unknown_mode_is_rejected_with_the_alternatives() {
        let tool = write_tool(fs_handle());
        let result = tool
            .execute(call(
                json!({ "file_path": "a.txt", "content": "x", "mode": "append" }),
            ))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("an unknown mode is a failure");
        };
        assert!(message.contains("append"), "{message}");
        assert!(message.contains("create"), "{message}");
        assert!(message.contains("overwrite"), "{message}");
    }

    #[test]
    fn mode_is_required_rather_than_defaulted() {
        let tool = write_tool(fs_handle());
        let schema = tool.schema().parameters.to_string();
        // The schema must demand `mode`, because treating an absent mode as
        // "overwrite" would destroy a file on a malformed call.
        assert!(schema.contains("\"mode\""), "{schema}");
        assert!(schema.contains("\"create\""), "{schema}");
        assert!(schema.contains("\"overwrite\""), "{schema}");
    }
}
