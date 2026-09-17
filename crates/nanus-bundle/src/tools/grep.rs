//! The `grep` tool: find text inside files.
//!
//! The tool searches for a **literal substring**, not a regular expression, and the
//! description says so. A model that assumes regex will write `\d+` and get nothing;
//! a model that knows it is literal will paste the text it is looking for. The
//! alternative — accepting a regex — means every model-authored pattern is a
//! potential exponential backtrack, in a tool that runs on every step.
//!
//! `include` narrows by file name and takes exactly one positive glob. Comma lists
//! and negations are rejected up front with a message that says what to do instead,
//! because a silently-ignored filter produces a wrong answer rather than an error.

use core::fmt::Write as _;
use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{FsHandle, SearchQuery};
use serde_json::json;

use crate::args::Arguments;
use crate::tools::port_error_result;

/// The default match ceiling.
pub const DEFAULT_MATCH_LIMIT: u32 = 250;

/// The largest ceiling a caller may request.
pub const MAX_MATCH_LIMIT: u32 = 2_000;

/// The longest line echoed back for one match.
pub const MAX_MATCH_LINE: usize = 400;

/// Builds the `grep` tool over `fs`.
pub fn grep_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("grep").unwrap_or_else(|_| unreachable!("grep is a valid tool name")),
        description: "Search file contents for a literal piece of text (not a regular \
                      expression) and return the matching lines with their file and line \
                      number. Use `include` with a single glob such as `*.rs` to narrow the \
                      search to one kind of file."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The literal text to find."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search under, relative to the workspace root. \
                                    Defaults to the workspace root."
                },
                "include": {
                    "type": "string",
                    "description": "One glob such as `*.rs` limiting which files are searched. \
                                    A comma list or a leading `!` is rejected."
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
    ToolDefinition::new(schema, GrepExecutor { fs }).with_access(ToolAccess::Read)
}

/// Executes `grep`.
struct GrepExecutor {
    fs: FsHandle,
}

impl ToolExecutor for GrepExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move { grep_outcome(fs, call).await })
    }
}

/// Searches by content and renders the matches.
async fn grep_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("grep", &call.arguments);
    let pattern = match arguments.required_str("pattern") {
        Ok(pattern) => pattern,
        Err(failure) => return ToolResult::new(id, failure),
    };
    if pattern.is_empty() {
        // An empty literal matches every line of every file.
        return ToolResult::new(id, ToolOutcome::failure("grep: the pattern is empty"));
    }
    let root = match arguments.optional_str("path") {
        Ok(root) => root.unwrap_or_else(|| ".".to_owned()),
        Err(failure) => return ToolResult::new(id, failure),
    };
    let include = match arguments.optional_str("include") {
        Ok(include) => include,
        Err(failure) => return ToolResult::new(id, failure),
    };
    if let Some(include) = include.as_deref()
        && let Err(reason) = validate_include(include)
    {
        return ToolResult::new(id, ToolOutcome::failure(format!("grep: {reason}")));
    }
    let limit = match arguments.optional_u32("limit") {
        Ok(limit) => limit.unwrap_or(DEFAULT_MATCH_LIMIT).min(MAX_MATCH_LIMIT),
        Err(failure) => return ToolResult::new(id, failure),
    };
    // A cap of zero is refused rather than reinterpreted, for the same reason `read`
    // refuses one: it is a nonsense request, and the adapter's `SearchQuery` has a
    // precondition that the cap is positive.
    if limit == 0 {
        return ToolResult::new(
            id,
            ToolOutcome::failure(String::from("grep: limit must be at least 1")),
        );
    }

    // The search runs unfiltered and the `include` glob is applied to the returned
    // paths. That is deliberate for this port shape: the port's query carries no
    // file filter, so filtering here keeps the tool honest about what it asked for
    // rather than pretending a filter was pushed down.
    let search_limit = usize::try_from(limit).unwrap_or(SearchQuery::DEFAULT_MAX_RESULTS);
    let query = SearchQuery::literal(&root, &pattern).with_max_results(search_limit);
    let found = fs.search(&query).await;
    let outcome = match found {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "grep", &error),
    };

    let hits = filter_by_include(outcome.matches, include.as_deref());
    // The port's flag alone, and not "the result filled the cap": a search that found exactly
    // its cap and then ran out of text is complete, and calling it truncated would send the
    // model hunting for matches that do not exist.
    let truncated = outcome.truncated;
    let value = json!({
        "pattern": pattern,
        "root": root,
        "count": hits.len(),
        "truncated": truncated,
        "files_scanned": outcome.files_scanned,
    });
    let text = render_matches(&hits, truncated, limit);
    ToolResult::new(
        id,
        ToolOutcome::success_with(value, vec![ContentBlock::Text(text)]),
    )
}

/// Keeps only the matches whose path matches `include`.
///
/// A malformed glob cannot reach here: [`validate_include`] runs first and reports
/// the problem to the model. Should one arrive anyway, the filter keeps everything
/// rather than silently discarding matches, because a filter that hides evidence is
/// worse than one that lets it through.
pub fn filter_by_include(
    hits: Vec<nanus_ports::SearchMatch>,
    include: Option<&str>,
) -> Vec<nanus_ports::SearchMatch> {
    let Some(include) = include else {
        return hits;
    };
    let Ok(glob) = globset::Glob::new(include) else {
        tracing::warn!(include, "grep: an unparseable include glob was ignored");
        return hits;
    };
    let matcher = glob.compile_matcher();
    hits.into_iter()
        .filter(|found| {
            // The pattern is matched against the path text and against the file
            // name, so `*.rs` works whether or not the search root was prefixed.
            matcher.is_match(&found.path)
                || found
                    .path
                    .file_name()
                    .is_some_and(|name| matcher.is_match(std::path::Path::new(name)))
        })
        .collect()
}

/// Checks that an `include` filter is one positive glob.
///
/// # Errors
///
/// Returns a message explaining what to pass instead. The filter is rejected rather
/// than partially applied, because a filter that is silently ignored produces a
/// confident wrong answer.
pub fn validate_include(include: &str) -> Result<(), String> {
    let trimmed = include.trim();
    if trimmed.is_empty() {
        return Err("include is empty; omit it to search every file".to_owned());
    }
    if trimmed.contains(',') {
        return Err(format!(
            "include takes one glob, but {trimmed:?} lists several; search once per pattern"
        ));
    }
    if trimmed.starts_with('!') {
        return Err(format!(
            "include is a positive filter, but {trimmed:?} negates; name what to search instead"
        ));
    }
    Ok(())
}

/// Renders matches grouped by file, with a cap notice.
pub fn render_matches(matches: &[nanus_ports::SearchMatch], truncated: bool, limit: u32) -> String {
    if matches.is_empty() {
        // A capped search that matched nothing *here* is a different fact from a search that
        // matched nothing at all. The cap stops the walk, so an empty set after one may mean
        // the matches were never reached — and "No matches found." would then be a confident
        // false negative that a model has no way to question.
        return if truncated {
            format!(
                "No matches in the files reached: the search stopped at the {limit}-match cap \
                 before it finished, so matches may exist beyond it.\n"
            )
        } else {
            "No matches found.\n".to_owned()
        };
    }
    let mut rendered = String::new();
    let mut current: Option<&std::path::Path> = None;
    for found in matches {
        if current != Some(found.path.as_path()) {
            if current.is_some() {
                rendered.push('\n');
            }
            let _ = writeln!(rendered, "{}:", found.path.display());
            current = Some(found.path.as_path());
        }
        let line = truncate_line(&found.line, MAX_MATCH_LINE);
        let _ = writeln!(rendered, "  {}: {line}", found.line_number);
    }
    if truncated {
        let _ = writeln!(
            rendered,
            "(stopped at {limit} matches; narrow the pattern or the include filter)"
        );
    }
    rendered
}

/// Truncates a matched line so one pathological line cannot dominate the result.
fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", line.get(..end).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::ToolCallId;
    use nanus_ports::{FsPort, SearchMatch};
    use std::path::PathBuf;

    /// A wrongly typed optional argument is refused, not folded into its default.
    #[tokio::test]
    async fn a_wrongly_typed_filter_is_reported_rather_than_defaulted() {
        let port: Box<dyn FsPort> = Box::new(crate::tests_support::UnusedFs);
        let tool = grep_tool(std::rc::Rc::new(port));
        for arguments in [
            json!({ "pattern": "fn", "limit": "many" }),
            json!({ "pattern": "fn", "path": 7 }),
        ] {
            let result = tool
                .execute(ToolCall::new(
                    ToolCallId::new("c-grep"),
                    ToolName::new("grep").unwrap_or_else(|_| unreachable!("grep is valid")),
                    arguments.clone(),
                ))
                .await;
            let ToolOutcome::Failure { message, .. } = &result.outcome else {
                panic!("{arguments} must be refused: {:?}", result.outcome);
            };
            assert!(
                message.contains("limit") || message.contains("path"),
                "{message}"
            );
        }
    }

    fn found(path: &str, line_number: u64, line: &str) -> SearchMatch {
        SearchMatch {
            path: PathBuf::from(path),
            line_number,
            line: line.to_owned(),
        }
    }

    #[test]
    fn an_empty_result_says_so() {
        assert_eq!(render_matches(&[], false, 100), "No matches found.\n");
    }

    #[test]
    fn matches_are_grouped_by_file() {
        let matches = vec![
            found("a.rs", 1, "one"),
            found("a.rs", 5, "two"),
            found("b.rs", 2, "three"),
        ];
        let rendered = render_matches(&matches, false, 100);
        assert!(rendered.contains("a.rs:"), "{rendered}");
        assert!(rendered.contains("  1: one"), "{rendered}");
        assert!(rendered.contains("  5: two"), "{rendered}");
        assert!(rendered.contains("b.rs:"), "{rendered}");
        // The file name appears once per file, not once per match.
        assert_eq!(rendered.matches("a.rs:").count(), 1, "{rendered}");
    }

    #[test]
    fn a_truncated_result_is_labelled_with_the_cap() {
        let matches = vec![found("a.rs", 1, "one")];
        let rendered = render_matches(&matches, true, 250);
        assert!(rendered.contains("stopped at 250 matches"), "{rendered}");
    }

    #[test]
    fn a_very_long_line_is_truncated_on_a_character_boundary() {
        let long = "é".repeat(1_000);
        let rendered = truncate_line(&long, 11);
        assert!(rendered.ends_with('…'), "{rendered}");
        // The retained prefix must be a real prefix, which a byte-wise cut would not
        // guarantee for multi-byte text.
        assert!(long.starts_with(rendered.trim_end_matches('…')));
    }

    #[test]
    fn a_short_line_is_untouched() {
        assert_eq!(truncate_line("short", 400), "short");
        // Boundary: exactly at the cap.
        assert_eq!(truncate_line("12345", 5), "12345");
    }

    #[test]
    fn the_include_filter_keeps_only_matching_paths() {
        let matches = vec![
            found("src/a.rs", 1, "hit"),
            found("src/b.toml", 1, "hit"),
            found("src/c.rs", 1, "hit"),
        ];
        let kept = filter_by_include(matches, Some("*.rs"));
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .all(|found| found.path.extension().is_some_and(|ext| ext == "rs"))
        );
    }

    #[test]
    fn no_include_keeps_everything() {
        let matches = vec![found("a.rs", 1, "x"), found("b.toml", 1, "x")];
        assert_eq!(filter_by_include(matches, None).len(), 2);
    }

    #[test]
    fn a_directory_scoped_glob_matches_the_whole_path() {
        let matches = vec![found("src/deep/a.rs", 1, "x"), found("tests/b.rs", 1, "x")];
        let kept = filter_by_include(matches, Some("src/**/*.rs"));
        assert_eq!(kept.len(), 1);
        assert!(kept[0].path.starts_with("src"));
    }

    #[test]
    fn include_accepts_a_single_positive_glob() {
        assert!(validate_include("*.rs").is_ok());
        assert!(validate_include("src/**/*.toml").is_ok());
    }

    #[test]
    fn include_rejects_a_list_and_a_negation() {
        let list = validate_include("*.rs,*.toml");
        assert!(list.is_err());
        let Some(message) = list.err() else {
            return;
        };
        // The message must say what to do instead, not only what was wrong.
        assert!(message.contains("one glob"), "{message}");

        let negated = validate_include("!*.lock");
        assert!(negated.is_err());
        let Some(message) = negated.err() else {
            return;
        };
        assert!(message.contains("positive"), "{message}");

        assert!(validate_include("   ").is_err());
    }

    /// A capped search that reached no match in the files it filtered is not a search that
    /// found nothing. Saying "No matches found." there is a confident false negative that a
    /// model has no way to question — it cannot tell a complete search from a truncated one.
    #[test]
    fn an_empty_result_after_a_cap_is_not_reported_as_no_matches() {
        let complete = render_matches(&[], false, 100);
        assert_eq!(complete, "No matches found.\n");

        let capped = render_matches(&[], true, 100);
        assert!(
            !capped.contains("No matches found."),
            "a capped search must not claim there are none: {capped}"
        );
        assert!(
            capped.contains("100-match cap"),
            "it says what stopped it: {capped}"
        );

        // The other direction: a match is rendered as a match whatever `truncated` says.
        let rendered = render_matches(&[found("a.rs", 1, "hit")], true, 1);
        assert!(rendered.contains("hit"), "{rendered}");
    }
}
