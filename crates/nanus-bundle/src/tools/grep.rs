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
//! because a silently-ignored filter produces a wrong answer rather than an error. The
//! glob is part of the port's query, so it narrows the files searched *before* the match
//! cap rather than the matches returned after it.
//!
//! Files over the size cap, binary files and unreadable files are passed over, counted,
//! and reported as partial coverage: a search that skipped a file cannot say a match is
//! absent from it, and the large ones can still be read in byte windows.

use core::fmt::Write as _;
use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{FsHandle, SearchOutcome, SearchQuery};
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

    // The `include` glob travels in the query, so the port applies it before its match cap.
    // It used to be applied here, to the capped result, and then matches in excluded files
    // could fill the cap and leave the one included match unreturned — an empty answer to a
    // question that had one.
    let search_limit = usize::try_from(limit).unwrap_or(SearchQuery::DEFAULT_MAX_RESULTS);
    let mut query = SearchQuery::literal(&root, &pattern).with_max_results(search_limit);
    if let Some(include) = include.as_deref() {
        query = query.with_include(include.trim());
    }
    let found = fs.search(&query).await;
    let outcome = match found {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "grep", &error),
    };

    // The port's flag alone, and not "the result filled the cap": a search that found exactly
    // its cap and then ran out of text is complete, and calling it truncated would send the
    // model hunting for matches that do not exist.
    let value = json!({
        "pattern": pattern,
        "root": root,
        "count": outcome.matches.len(),
        "truncated": outcome.truncated,
        "files_scanned": outcome.files_scanned,
        "skipped_large": outcome.skipped_large,
        "skipped_binary": outcome.skipped_binary,
        "skipped_unreadable": outcome.skipped_unreadable,
        "coverage": outcome.coverage().as_str(),
    });
    let text = render_outcome(&outcome, limit, query.max_file_bytes);
    ToolResult::new(
        id,
        ToolOutcome::success_with(value, vec![ContentBlock::Text(text)]),
    )
}

/// Renders a search's matches and, when it passed files over, what it did not search.
///
/// With nothing skipped this is exactly [`render_matches`]. With a file skipped, an empty
/// result says "in the files searched" rather than "found", because a file over the size
/// cap can hold the match, and the note says how to look there.
pub fn render_outcome(outcome: &SearchOutcome, limit: u32, max_file_bytes: u64) -> String {
    let mut text = if outcome.matches.is_empty() && !outcome.truncated && outcome.skipped() > 0 {
        "No matches in the files searched.\n".to_owned()
    } else {
        render_matches(&outcome.matches, outcome.truncated, limit)
    };
    if let Some(note) = render_skipped(outcome, max_file_bytes) {
        text.push_str(&note);
    }
    text
}

/// Describes the files a search passed over, or nothing when it passed over none.
fn render_skipped(outcome: &SearchOutcome, max_file_bytes: u64) -> Option<String> {
    if outcome.skipped() == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if outcome.skipped_large > 0 {
        parts.push(format!(
            "{} over the {max_file_bytes}-byte size cap (read those in byte windows with \
             read's byte_offset and max_bytes)",
            outcome.skipped_large
        ));
    }
    if outcome.skipped_binary > 0 {
        parts.push(format!("{} binary or not UTF-8", outcome.skipped_binary));
    }
    if outcome.skipped_unreadable > 0 {
        parts.push(format!("{} unreadable", outcome.skipped_unreadable));
    }
    Some(format!(
        "(coverage partial: {} files not searched: {})\n",
        outcome.skipped(),
        parts.join("; ")
    ))
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

    /// Runs `grep` with `arguments` over `fs`.
    async fn grep(fs: &crate::tests_support::MemoryFs, arguments: serde_json::Value) -> ToolResult {
        grep_tool(fs.handle())
            .execute(ToolCall::new(
                ToolCallId::new("c-grep"),
                ToolName::new("grep").unwrap_or_else(|_| unreachable!("grep is valid")),
                arguments,
            ))
            .await
    }

    /// The filter travels in the query, so the port applies it before its cap — and every
    /// match the port returns is shown, with none dropped afterwards by a second filter.
    #[tokio::test]
    async fn the_include_glob_is_sent_to_the_port_rather_than_applied_to_its_answer() {
        let answer = SearchOutcome {
            matches: vec![found("/w/src/deep/a.rs", 3, "hit")],
            files_scanned: 1,
            ..SearchOutcome::default()
        };
        let fs = crate::tests_support::MemoryFs::new("").answer_searches_with(answer);
        let result = grep(&fs, json!({ "pattern": "hit", "include": " src/**/*.rs " })).await;
        let queries = fs.queries();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].include.as_deref(), Some("src/**/*.rs"));
        let text = result.outcome.render_text();
        assert!(text.contains("3: hit"), "the port's match is shown: {text}");

        // The other direction: no include, no filter in the query.
        let _ = grep(&fs, json!({ "pattern": "hit" })).await;
        assert_eq!(fs.queries()[1].include, None);
    }

    #[tokio::test]
    async fn a_search_that_skipped_files_reports_partial_coverage_and_one_that_did_not_is_complete()
    {
        let complete = crate::tests_support::MemoryFs::new("");
        let result = grep(&complete, json!({ "pattern": "hit" })).await;
        let value = result.outcome.value().cloned().unwrap_or_default();
        assert_eq!(value["coverage"], "complete");
        assert_eq!(result.outcome.render_text(), "No matches found.\n");

        let skipped = SearchOutcome {
            skipped_large: 2,
            skipped_binary: 1,
            ..SearchOutcome::default()
        };
        let partial = crate::tests_support::MemoryFs::new("").answer_searches_with(skipped);
        let result = grep(&partial, json!({ "pattern": "hit" })).await;
        let value = result.outcome.value().cloned().unwrap_or_default();
        assert_eq!(value["coverage"], "partial");
        assert_eq!(value["skipped_large"], 2);
        assert_eq!(value["skipped_binary"], 1);
        assert_eq!(value["skipped_unreadable"], 0);
        let text = result.outcome.render_text();
        assert!(
            !text.contains("No matches found."),
            "a search that skipped files cannot claim there are none: {text}"
        );
        assert!(
            text.contains("coverage partial: 3 files not searched"),
            "{text}"
        );
        assert!(
            text.contains("byte_offset"),
            "the diagnostic names byte windows: {text}"
        );
        assert!(text.contains("1 binary"), "{text}");
    }

    #[test]
    fn an_outcome_with_nothing_skipped_renders_as_the_matches_alone() {
        let outcome = SearchOutcome {
            matches: vec![found("a.rs", 1, "one")],
            ..SearchOutcome::default()
        };
        assert_eq!(
            render_outcome(&outcome, 100, 10),
            render_matches(&outcome.matches, false, 100)
        );
        let skipped = SearchOutcome {
            skipped_unreadable: 1,
            ..outcome
        };
        let rendered = render_outcome(&skipped, 100, 10);
        assert!(rendered.contains("one"), "{rendered}");
        assert!(rendered.contains("1 unreadable"), "{rendered}");
        assert!(
            !rendered.contains("size cap"),
            "only the reasons that apply: {rendered}"
        );
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
