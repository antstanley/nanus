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
//!
//! The result is the diff, one hunk per changed stretch and bounded as a whole, and a file
//! above the size `read` will show by lines is refused before it is touched: a tool whose
//! result could be too large to record would be one that changes a file and then reports
//! that it failed.

use core::fmt::Write as _;
use core::ops::Range;

use nanus_domain::{
    ContentBlock, Destructive, ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{EditOutcome, FsHandle};
use serde_json::json;

use crate::args::Arguments;
use crate::tools::port_error_result;
use crate::tools::read::MAX_READ_BYTES;

/// The most diff text one edit renders.
///
/// An edit's result is read on every later request, and a `replace_all` over a common word
/// can change hundreds of lines. Past this the diff says how much of itself it is showing;
/// the file is the place to look at the rest.
pub const MAX_DIFF_BYTES: usize = 16 * 1024;

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
    ToolDefinition::new(schema, EditExecutor { fs })
        .with_access(ToolAccess::Write)
        .with_destructive(Destructive::Never)
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

/// What an `edit` call asked for, after its arguments were checked.
struct EditRequest {
    path: String,
    old: String,
    new: String,
    replace_all: bool,
}

/// Reads and checks an `edit` call's arguments.
fn edit_request(arguments: &Arguments<'_>) -> Result<EditRequest, ToolOutcome> {
    let request = EditRequest {
        path: arguments.required_str("file_path")?,
        old: arguments.required_str("old_string")?,
        new: arguments.required_str("new_string")?,
        replace_all: arguments.flag("replace_all")?,
    };
    if request.old.is_empty() {
        // Replacing the empty string would insert at every position, which no model
        // means by it.
        return Err(ToolOutcome::failure(
            "edit: old_string is empty; pass the text to replace",
        ));
    }
    if request.old == request.new {
        // A no-op edit usually means the model has lost track of the file's contents.
        return Err(ToolOutcome::failure(
            "edit: old_string and new_string are identical, so nothing changes",
        ));
    }
    Ok(request)
}

/// Performs the edit and renders the outcome.
async fn edit_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("edit", &call.arguments);
    let request = match edit_request(&arguments) {
        Ok(request) => request,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let path = std::path::Path::new(&request.path);

    // Size is checked before the edit, as `read` checks it before a read. An edit holds the
    // file twice over — before and after — and a file `read` refuses to show is one the
    // model cannot have looked at before changing. Refusing here is also what keeps "the
    // edit was applied" and "the result was accepted" the same fact: nothing this tool
    // renders for a file under the ceiling can outgrow a session record.
    match fs.metadata(path).await {
        Ok(meta) if meta.byte_len > MAX_READ_BYTES => {
            return ToolResult::new(
                id,
                ToolOutcome::failure(format!(
                    "edit: {} is {} bytes, above the {MAX_READ_BYTES}-byte ceiling for an \
                     edit; nothing was changed. Change it with a command instead",
                    request.path, meta.byte_len
                )),
            );
        }
        Ok(_) => {}
        Err(error) => return port_error_result(id, "edit", &error),
    }

    let edited = fs
        .edit(path, &request.old, &request.new, request.replace_all)
        .await;
    let outcome = match edited {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "edit", &error),
    };

    let diff = render_diff(&outcome, &request.old, &request.new);
    let value = json!({
        "file_path": request.path,
        "replacements": outcome.replacements,
        "bytes_before": outcome.before.len(),
        "bytes_after": outcome.after.len(),
    });
    // The diff is the model-visible content: it shows what changed, which is the
    // thing the model must verify before its next step.
    let text = format!(
        "Edited {} ({} replacement{}).\n{diff}",
        request.path,
        outcome.replacements,
        if outcome.replacements == 1 { "" } else { "s" }
    );
    ToolResult::new(
        id,
        ToolOutcome::success_with(value, vec![ContentBlock::Text(text)]),
    )
}

/// One changed stretch of the file: the whole lines a replacement touched, on each side.
///
/// Both ranges are byte ranges that start at a line start and end at a line end, so each
/// can be cut out and shown as lines.
#[derive(Debug, PartialEq, Eq)]
struct Region {
    before: Range<usize>,
    after: Range<usize>,
}

/// The byte offset at which the line containing `at` starts.
fn line_start(text: &str, at: usize) -> usize {
    text.get(..at)
        .and_then(|head| head.rfind('\n'))
        .map_or(0, |newline| newline.saturating_add(1))
}

/// The byte offset of the newline that ends the line containing `at`, or the text's end.
fn line_end(text: &str, at: usize) -> usize {
    text.get(at..)
        .and_then(|tail| tail.find('\n'))
        .map_or(text.len(), |newline| at.saturating_add(newline))
}

/// Finds the lines each replacement touched, in order, merging the ones that share a line.
///
/// The edit is known exactly — `old` was replaced by `new` at the first `count` places it
/// occurs — so the regions are computed from the occurrences rather than by comparing the two
/// texts: the k-th occurrence sits in the result where it sat before, moved by what the k
/// replacements ahead of it removed and added.
fn changed_regions(before: &str, after: &str, old: &str, new: &str, count: usize) -> Vec<Region> {
    let mut regions: Vec<Region> = Vec::new();
    for (index, (start, _)) in before.match_indices(old).take(count).enumerate() {
        let end = start.saturating_add(old.len());
        let moved = start
            .saturating_sub(index.saturating_mul(old.len()))
            .saturating_add(index.saturating_mul(new.len()));
        let region = Region {
            before: line_start(before, start)..line_end(before, end),
            after: line_start(after, moved)..line_end(after, moved.saturating_add(new.len())),
        };
        match regions.last_mut() {
            Some(last) if region.before.start <= last.before.end => {
                last.before.end = last.before.end.max(region.before.end);
                last.after.end = last.after.end.max(region.after.end);
            }
            _ => regions.push(region),
        }
    }
    regions
}

/// How many newlines `text` holds between two byte offsets.
fn newlines_between(text: &str, from: usize, to: usize) -> usize {
    text.get(from..to)
        .map_or(0, |span| span.matches('\n').count())
}

/// Renders one region as a hunk, given the 1-based line each side of it starts on.
///
/// Lines the two sides share at the top and the bottom are left out, so a hunk shows what
/// changed and not the unchanged line a multi-line pattern happened to span.
fn render_region(before: &str, after: &str, before_line: usize, after_line: usize) -> String {
    let removed: Vec<&str> = before.split('\n').collect();
    let added: Vec<&str> = after.split('\n').collect();
    let shared = removed.len().min(added.len());
    let head = removed
        .iter()
        .zip(&added)
        .take_while(|(left, right)| left == right)
        .count();
    let tail = removed
        .iter()
        .rev()
        .zip(added.iter().rev())
        .take(shared.saturating_sub(head))
        .take_while(|(left, right)| left == right)
        .count();
    let removed = removed
        .get(head..removed.len().saturating_sub(tail))
        .unwrap_or_default();
    let added = added
        .get(head..added.len().saturating_sub(tail))
        .unwrap_or_default();
    let mut hunk = format!(
        "@@ -{},{} +{},{} @@\n",
        before_line.saturating_add(head),
        removed.len(),
        after_line.saturating_add(head),
        added.len()
    );
    for line in removed {
        let _ = writeln!(hunk, "-{line}");
    }
    for line in added {
        let _ = writeln!(hunk, "+{line}");
    }
    hunk
}

/// Renders what an edit changed as one hunk per changed stretch, within [`MAX_DIFF_BYTES`].
///
/// One hunk from the first change to the last is what this used to be, and for `replace_all`
/// that is every line between two occurrences shown as removed and added again: an edit of
/// two words at either end of a file came back as the file, twice. The budget is for the edit
/// with many occurrences, and the notice that ends a cut diff says the file was changed in
/// full, because a model shown part of a diff must not conclude the rest did not happen.
fn render_diff(outcome: &EditOutcome, old: &str, new: &str) -> String {
    let count = usize::try_from(outcome.replacements).unwrap_or(usize::MAX);
    let regions = changed_regions(&outcome.before, &outcome.after, old, new, count);
    let path = outcome.path.display();
    let mut diff = format!("--- a/{path}\n+++ b/{path}\n");
    let (mut before_at, mut before_line) = (0_usize, 1_usize);
    let (mut after_at, mut after_line) = (0_usize, 1_usize);
    let mut shown = 0_usize;
    for region in &regions {
        let (before, after) = (&outcome.before, &outcome.after);
        before_line =
            before_line.saturating_add(newlines_between(before, before_at, region.before.start));
        after_line =
            after_line.saturating_add(newlines_between(after, after_at, region.after.start));
        before_at = region.before.start;
        after_at = region.after.start;
        let hunk = render_region(
            before.get(region.before.clone()).unwrap_or_default(),
            after.get(region.after.clone()).unwrap_or_default(),
            before_line,
            after_line,
        );
        let room = MAX_DIFF_BYTES.saturating_sub(diff.len());
        if hunk.len() > room {
            if shown == 0 {
                // One change larger than the budget: its start is still worth showing.
                diff.push_str(
                    hunk.get(..floor_char_boundary(&hunk, room))
                        .unwrap_or_default(),
                );
                diff.push('\n');
                shown = 1;
            }
            let _ = writeln!(
                diff,
                "(diff cut at {MAX_DIFF_BYTES} bytes: {shown} of {} changed regions shown; \
                 every replacement was applied)",
                regions.len()
            );
            break;
        }
        diff.push_str(&hunk);
        shown = shown.saturating_add(1);
    }
    diff
}

/// The largest character boundary of `text` at or below `at`.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut end = at.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
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

    /// A wrongly typed `replace_all` is refused rather than read as false, which would edit
    /// one occurrence of a pattern the model asked to replace everywhere.
    #[tokio::test]
    async fn a_wrongly_typed_replace_all_is_reported_rather_than_defaulted() {
        let definition = edit_tool(fs_handle());
        let result = definition
            .execute(call(json!({
                "file_path": "a.txt",
                "old_string": "a",
                "new_string": "b",
                "replace_all": "yes"
            })))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("a string is not a boolean: {:?}", result.outcome);
        };
        assert!(message.contains("replace_all"), "{message}");
        assert!(message.contains("boolean"), "{message}");
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

    /// Builds the outcome the port would return for replacing `old` by `new` in `before`.
    fn edited(before: &str, old: &str, new: &str, replace_all: bool) -> EditOutcome {
        let (after, replacements) = if replace_all {
            (before.replace(old, new), before.matches(old).count())
        } else {
            (before.replacen(old, new, 1), 1)
        };
        EditOutcome {
            path: std::path::PathBuf::from("f.txt"),
            replacements: u32::try_from(replacements).unwrap_or(u32::MAX),
            before: before.to_owned(),
            after,
        }
    }

    #[test]
    fn a_single_replacement_is_one_hunk_of_the_lines_it_changed() {
        let diff = render_diff(&edited("a\nb\nc\n", "b", "B", false), "b", "B");
        assert_eq!(diff, "--- a/f.txt\n+++ b/f.txt\n@@ -2,1 +2,1 @@\n-b\n+B\n");
        // A pattern spanning an unchanged line leaves that line out of the hunk.
        let diff = render_diff(&edited("a\nb\nc\n", "a\nb", "a\nB", false), "a\nb", "a\nB");
        assert!(diff.ends_with("@@ -2,1 +2,1 @@\n-b\n+B\n"), "{diff}");
        // A deleted line is a removal with nothing added, and an added one the reverse.
        let diff = render_diff(&edited("a\nb\nc\n", "b\n", "", false), "b\n", "");
        assert!(diff.ends_with("@@ -2,1 +2,0 @@\n-b\n"), "{diff}");
        let diff = render_diff(&edited("a\nc\n", "a\n", "a\nb\n", false), "a\n", "a\nb\n");
        assert!(diff.ends_with("@@ -2,0 +2,1 @@\n+b\n"), "{diff}");
    }

    /// The defect this pins: one hunk from the first change to the last showed every line
    /// between two occurrences as removed and added again.
    #[test]
    fn replacements_far_apart_are_separate_hunks_without_the_lines_between() {
        let mut before = String::from("old first\n");
        for line in 0..500 {
            let _ = writeln!(before, "unchanged {line}");
        }
        before.push_str("old last\nold old same line\n");
        let diff = render_diff(&edited(&before, "old", "new", true), "old", "new");
        assert!(!diff.contains("unchanged"), "{diff}");
        assert!(
            diff.contains("@@ -1,1 +1,1 @@\n-old first\n+new first\n"),
            "{diff}"
        );
        assert!(
            diff.contains("@@ -502,1 +502,1 @@\n-old last\n+new last\n"),
            "{diff}"
        );
        // Two occurrences on one line are one hunk, shown once.
        assert_eq!(diff.matches("+new new same line").count(), 1, "{diff}");
        assert_eq!(diff.matches("@@").count(), 6, "three hunks: {diff}");
        // A replacement that changes the line count moves the later hunk's new position.
        let diff = render_diff(&edited("x\nmid\nx\n", "x", "x\ny", true), "x", "x\ny");
        assert!(diff.contains("@@ -4,0 +5,1 @@\n+y\n"), "{diff}");
    }

    #[test]
    fn a_diff_past_its_budget_is_cut_and_says_the_edit_was_applied_in_full() {
        let before = "word of some length on a line\n".repeat(2_000);
        let diff = render_diff(&edited(&before, "word", "term", true), "word", "term");
        assert!(
            diff.len() <= MAX_DIFF_BYTES.saturating_add(256),
            "{}",
            diff.len()
        );
        assert!(diff.contains("of 2000 changed regions shown"), "{diff}");
        assert!(diff.contains("every replacement was applied"), "{diff}");
        // One change larger than the whole budget still shows its start.
        let huge = "é".repeat(MAX_DIFF_BYTES);
        let diff = render_diff(&edited("small\n", "small", &huge, false), "small", &huge);
        assert!(diff.contains("-small"), "{diff}");
        assert!(diff.contains("1 of 1 changed regions shown"), "{diff}");
        assert!(diff.len() <= MAX_DIFF_BYTES.saturating_add(256));
        // The other direction: a diff inside the budget carries no notice.
        let small = render_diff(&edited("a\n", "a", "b", false), "a", "b");
        assert!(!small.contains("diff cut"), "{small}");
    }

    /// A file `read` refuses to show by lines is refused here before the port is asked to
    /// change it; one under the ceiling reaches the port.
    #[tokio::test]
    async fn a_file_above_the_read_ceiling_is_refused_before_it_is_touched() {
        let arguments = json!({ "file_path": "big.txt", "old_string": "a", "new_string": "b" });
        let oversized = usize::try_from(MAX_READ_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        let large = crate::tests_support::MemoryFs::new(vec![b'a'; oversized]);
        let result = edit_tool(large.handle())
            .execute(call(arguments.clone()))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("an oversized file is refused: {:?}", result.outcome);
        };
        assert!(message.contains("ceiling"), "{message}");
        assert!(message.contains("nothing was changed"), "{message}");

        // The double's `edit` answers "does not exist", so that message means it was asked.
        let small = crate::tests_support::MemoryFs::new(b"a".to_vec());
        let result = edit_tool(small.handle()).execute(call(arguments)).await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("the double cannot edit: {:?}", result.outcome);
        };
        assert!(message.contains("does not exist"), "{message}");
    }
}
