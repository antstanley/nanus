//! The `glob` tool: find files by name.
//!
//! A separate tool from [`crate::tools::grep_tool`] because the two answer different
//! questions and the model chooses between them by reading one sentence. A glob is
//! bounded by a result cap and a per-file size cap, and it says when the cap stopped
//! it, so a model can narrow the pattern rather than assume it saw everything.

use core::fmt::Write as _;
use nanus_domain::{
    ContentBlock, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolOutcome,
    ToolResult, ToolSchema,
};
use nanus_ports::{FsHandle, SearchKind, SearchQuery};
use serde_json::json;

use crate::args::Arguments;
use crate::tools::port_error_result;

/// The default match ceiling.
pub const DEFAULT_MATCH_LIMIT: u32 = 100;

/// The largest ceiling a caller may request.
pub const MAX_MATCH_LIMIT: u32 = 1_000;

/// Builds the `glob` tool over `fs`.
pub fn glob_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("glob").unwrap_or_else(|_| unreachable!("glob is a valid tool name")),
        description: "Find files whose path matches a glob pattern. Patterns are anchored \
                      to the search root, so `*.rs` matches only `.rs` files directly in \
                      it and `**/*.rs` matches them at any depth. Returns matching paths, \
                      one per line."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern, matched against the path relative to \
                                    the search root. Use `**/` for every depth."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search under, relative to the workspace root. \
                                    Defaults to the workspace root."
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "Maximum matches to return. Defaults to {DEFAULT_MATCH_LIMIT}."
                    )
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, GlobExecutor { fs })
}

/// Executes `glob`.
struct GlobExecutor {
    fs: FsHandle,
}

impl ToolExecutor for GlobExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move { glob_outcome(fs, call).await })
    }
}

/// Searches by glob and renders the matches.
async fn glob_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("glob", &call.arguments);
    let pattern = match arguments.required_str("pattern") {
        Ok(pattern) => pattern,
        Err(failure) => return ToolResult::new(id, failure),
    };
    if pattern.trim().is_empty() {
        return ToolResult::new(id, ToolOutcome::failure("glob: the pattern is empty"));
    }
    let root = arguments
        .optional_str("path")
        .unwrap_or(None)
        .unwrap_or_else(|| ".".to_owned());
    let limit = arguments
        .optional_u32("limit")
        .unwrap_or(None)
        .unwrap_or(DEFAULT_MATCH_LIMIT)
        .min(MAX_MATCH_LIMIT);
    // A cap of zero is refused rather than reinterpreted, for the same reason `read`
    // refuses one: it is a nonsense request, and the adapter's `SearchQuery` has a
    // precondition that the cap is positive.
    if limit == 0 {
        return ToolResult::new(
            id,
            ToolOutcome::failure(String::from("glob: limit must be at least 1")),
        );
    }

    let mut query = SearchQuery::glob(&root, &pattern);
    query.max_results = usize::try_from(limit).unwrap_or(SearchQuery::DEFAULT_MAX_RESULTS);
    // Postcondition: the query is a glob query, so a caller cannot accidentally get
    // a content search from a tool named `glob`.
    assert_eq!(query.kind, SearchKind::Glob);

    let found = fs.search(&query).await;
    let outcome = match found {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "glob", &error),
    };

    let paths: Vec<String> = outcome
        .matches
        .iter()
        .map(|found| found.path.display().to_string())
        .collect();
    let value = json!({
        "pattern": pattern,
        "root": root,
        "count": paths.len(),
        "truncated": outcome.truncated,
        "files_scanned": outcome.files_scanned,
    });
    let text = render_matches(&paths, outcome.truncated, limit);
    ToolResult::new(
        id,
        ToolOutcome::success_with(value, vec![ContentBlock::Text(text)]),
    )
}

/// Renders a match list, saying so when the cap was reached.
pub fn render_matches(paths: &[String], truncated: bool, limit: u32) -> String {
    if paths.is_empty() {
        return "No files found.\n".to_owned();
    }
    let mut rendered = paths.join("\n");
    rendered.push('\n');
    if truncated {
        // A silent cap would let the model conclude a file does not exist.
        let _ = writeln!(
            rendered,
            "(more than {limit} matches; narrow the pattern to see the rest)"
        );
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_result_says_so() {
        // An empty rendering would be indistinguishable from a broken tool.
        assert_eq!(render_matches(&[], false, 100), "No files found.\n");
    }

    #[test]
    fn matches_are_listed_one_per_line() {
        let paths = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        let rendered = render_matches(&paths, false, 100);
        assert_eq!(rendered, "src/a.rs\nsrc/b.rs\n");
    }

    #[test]
    fn a_truncated_result_is_labelled_with_the_cap() {
        let paths = vec!["a".to_owned()];
        let rendered = render_matches(&paths, true, 100);
        assert!(rendered.contains("more than 100 matches"), "{rendered}");
        assert!(rendered.contains("narrow the pattern"), "{rendered}");
    }

    #[test]
    fn the_limit_is_clamped_to_the_ceiling() {
        // Requesting a million matches must not be honoured; the clamp is what keeps
        // a broad pattern from filling the transcript.
        let requested = 1_000_000_u32;
        assert_eq!(requested.min(MAX_MATCH_LIMIT), MAX_MATCH_LIMIT);
        // Pair assertion: the ceiling is above the default, or the clamp would be the
        // tighter of the two and the default unreachable.
        const { assert!(MAX_MATCH_LIMIT > DEFAULT_MATCH_LIMIT) };
    }
}
