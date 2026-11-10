//! The one-line form: what a tool call and a thinking segment look like by default.
//!
//! A transcript is read for the answer. Everything else in it — which tool ran, what the
//! model thought on the way — is context, and context that is drawn in full buries the
//! thing a reader came for: a `read` occupies a paragraph of JSON, and a thinking segment
//! arrives as one long paragraph that is rewritten by every delta until it settles. The
//! default here is therefore one line each:
//!
//! ```text
//! ── thinking · the glob is anchored to the wrong directory, so let me check the caller
//! ⚙ Read File · crates/nanus-bundle/src/tools/glob.rs
//! ```
//!
//! The thinking line is the *newest* line of the segment, which is what makes it scroll:
//! each delta either extends the line or starts a new one, and the window onto it moves as
//! the model writes. The tool line names the tool in words and says what it is acting on.
//!
//! Two properties are deliberate. **A line never wraps** — it is clipped to the viewport
//! width, keeping the *end* of the text, because the newest words are the ones that say
//! what the model is doing now and the folder of a path is less informative than its file.
//! And **nothing here invents content**: every field comes from arguments the model sent,
//! and a call this module does not recognise is shown under its own name rather than
//! guessed at.
//!
//! [`Detail::Full`] is the way back to the whole call and the whole segment, for a reader
//! who wants to study the reasoning rather than skim it.

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// How much of a tool call and a thinking segment the transcript draws.
///
/// The default is [`Detail::Compact`], and the setting that changes it is `tui_detail` in
/// the configuration file — a reader who wants the whole of both asks for it once rather
/// than reaching for `Ctrl+T` and `Ctrl+R` on every turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Detail {
    /// One line per tool call, and the newest line of a thinking segment.
    #[default]
    Compact,
    /// The whole tool call, arguments included, and the whole thinking segment.
    Full,
}

/// How a tool call ended, which is what its line is marked with.
///
/// The mark lives on the call's line because in this form the call gets one line and no
/// other: the outcome has to be on it or it is nowhere. The three glyphs are the ones the
/// full form already uses, so switching forms does not mean learning a second vocabulary
/// for the same fact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolState {
    /// The call is running: no result answers it yet.
    Running,
    /// The call finished and reported success.
    Ok,
    /// The call finished and reported a failure.
    Failed,
}

impl ToolState {
    /// Returns the glyph this state is marked with.
    #[must_use]
    pub const fn mark(self) -> &'static str {
        match self {
            Self::Running => "⚙",
            Self::Ok => "✓",
            Self::Failed => "✗",
        }
    }
}

/// What a thinking line is prefixed with, so the dimmed line is not mistaken for the
/// answer.
const THINKING_PREFIX: &str = "── thinking";

/// Returns what a tool is called, in words rather than in the toolset's spelling.
///
/// The names are duplicated from the toolset on purpose. The interface links no toolset —
/// that is the split the two binaries are built around — so this is a table, and a tool it
/// has not been told about keeps its own name rather than being shown under a guess:
/// `⚙ fetch` is honest, and `⚙ Read File` for something that is not a read is not.
#[must_use]
pub fn tool_label(name: &str) -> &str {
    match name {
        "bash" => "Bash",
        "read" => "Read File",
        "write" => "Write File",
        "edit" => "Edit File",
        "read_image" => "Read Image",
        "glob" => "Glob",
        "grep" => "Grep",
        other => other,
    }
}

/// Builds the single line a tool call occupies at `width` columns.
#[must_use]
pub fn tool_line(state: ToolState, name: &str, arguments: &str, width: u16) -> String {
    let label = tool_label(name);
    let action = action_of(name, arguments);
    one_line(&format!("{} {label}", state.mark()), " · ", &action, width)
}

/// Draws the call a question is being asked about: one line that keeps the *start* of what it
/// is acting on.
///
/// An ordinary call's line is clipped from the front ([`tool_line`]), because the end of a long
/// path is the part that names the file. That is the wrong end of a command somebody is being
/// asked to allow. The approval dialog shows no arguments — the model wrote them, and the
/// question is the harness's — so this line is the one place the reader sees what the call
/// is, and clipped from the front it showed whatever the model put last: a command padded out
/// to the width read as its trailing comment. While the question is open the line is clipped
/// from the *end* instead, so what is read first is what runs first.
///
/// Every line of the argument is on it, joined with a mark where each break was, rather than
/// the first line alone: the second line of a command runs too. Control characters are
/// dropped. It is still one row, and still only the head of a long command — the whole of it
/// is the full detail's to show.
#[must_use]
pub fn asked_tool_line(state: ToolState, name: &str, arguments: &str, width: u16) -> String {
    let label = format!("{} {}", state.mark(), tool_label(name));
    let action = flattened(&whole_action(name, arguments));
    if action.is_empty() {
        return label;
    }
    head(&format!("{label} · {action}"), usize::from(width.max(1)))
}

/// The argument a call's line reports, whole: [`action_of`] without the cut to one line.
fn whole_action(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_owned();
    };
    let Some(object) = value.as_object() else {
        return String::new();
    };
    let Some(key) = action_key(name) else {
        return value.to_string();
    };
    match object.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Text on one line: each line break a visible mark, a tab a space, and no other control
/// character at all.
fn flattened(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.trim_end().chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                let _ = characters.next_if_eq(&'\n');
                out.push_str(" \u{23ce} ");
            }
            '\n' => out.push_str(" \u{23ce} "),
            '\t' => out.push(' '),
            other if other.is_control() => {}
            other => out.push(other),
        }
    }
    out
}

/// Returns the first `width` columns of `text`, ending in an ellipsis when some were dropped.
///
/// Columns rather than characters, and whole graphemes: this is the line a decision is read
/// from, so it is measured as it is drawn and never runs onto a second row.
fn head(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let room = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0_usize;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width();
        if used.saturating_add(cells) > room {
            break;
        }
        out.push_str(grapheme);
        used = used.saturating_add(cells);
    }
    out.push('…');
    out
}

/// Builds the single line a thinking segment occupies at `width` columns.
///
/// The segment's newest line, and a cursor while it is still arriving: a thinking segment
/// that has just started has no text yet, and a line drawn as blank would look like a
/// stalled turn rather than a model that is thinking.
#[must_use]
pub fn thinking_line(text: &str, streaming: bool, width: u16) -> String {
    // The newest line rather than the first: a segment is read while it is being written,
    // and its end is what the model is thinking now. `next_back` because a delta can end
    // with a newline, which leaves an empty last line the model is thinking *into*.
    let latest = text.split('\n').next_back().unwrap_or_default();
    // A streaming line reserves a column for the cursor, so that the cursor cannot be
    // what pushes it onto a second row.
    let usable = if streaming {
        width.saturating_sub(1)
    } else {
        width
    };
    let mut line = one_line(THINKING_PREFIX, " · ", latest, usable);
    if streaming {
        line.push('▌');
    }
    line
}

/// Returns what a call is acting on: a file, a command, a pattern.
fn action_of(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        // Arguments that are not JSON at all: a hand-written entry, or a call rendered by
        // some other producer. Showing them clipped beats showing nothing.
        return first_line(arguments);
    };
    let Some(object) = value.as_object() else {
        // `null` is what a frame from an agent that predates the arguments carries, and
        // "no arguments" is not something to say out loud.
        return String::new();
    };
    let Some(key) = action_key(name) else {
        // A tool this interface does not know: nothing is singled out, so the whole call
        // is shown rather than a field being guessed at.
        return first_line(&value.to_string());
    };
    object.get(key).map_or_else(String::new, value_text)
}

/// Returns the argument each known tool acts on.
fn action_key(name: &str) -> Option<&'static str> {
    match name {
        "bash" => Some("command"),
        "read" | "write" | "edit" | "read_image" => Some("file_path"),
        "glob" | "grep" => Some("pattern"),
        _ => None,
    }
}

/// Renders one argument value as the text of a line.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => first_line(text),
        other => first_line(&other.to_string()),
    }
}

/// Returns the first line of `text`, marked when it is not the whole of it.
///
/// A command or a pattern can be several lines, and one line is what this module promises.
fn first_line(text: &str) -> String {
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or_default().trim_end();
    if lines.any(|line| !line.trim().is_empty()) {
        return format!("{first} …");
    }
    first.to_owned()
}

/// Joins a label, a separator, and a body into one line of at most `width` columns.
///
/// The label is kept whatever happens — it is what says what the line *is* — and the body
/// is clipped from the front so that the newest words survive. A terminal narrower than the
/// label gets as much of the whole line as fits, because a line that shows only its label
/// says nothing at all.
///
/// The separator is a parameter rather than part of the label because a label with nothing
/// after it would otherwise end in a mark pointing at nothing.
fn one_line(label: &str, separator: &str, body: &str, width: u16) -> String {
    if body.is_empty() {
        return label.to_owned();
    }
    let width = usize::from(width.max(1));
    let head = format!("{label}{separator}");
    let head_width = head.chars().count();
    if head_width >= width {
        return tail(&format!("{head}{body}"), width);
    }
    format!("{head}{}", tail(body, width.saturating_sub(head_width)))
}

/// Returns the last `width` characters of `text`, prefixed with an ellipsis when some were
/// dropped.
///
/// Characters, not display columns. A line holding wide characters can therefore be wider
/// than the terminal, and the view breaks it onto a second row as it does any line that is
/// (`crate::rows`): the row is counted because it is made, so the scroll arithmetic and the
/// drawing still agree about how many rows the line takes.
fn tail(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(1);
    let dropped = count.saturating_sub(keep);
    let kept: String = text.chars().skip(dropped).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asked-about line is clipped from the end, by the columns it is drawn in, and is
    /// never wider than the row it has.
    #[test]
    fn an_asked_line_keeps_its_start_and_fits_the_width_in_cells() {
        let arguments = r#"{"command":"rm -rf 漢字漢字漢字漢字漢字 and a long tail of words"}"#;
        let line = asked_tool_line(ToolState::Running, "bash", arguments, 24);
        assert!(line.starts_with("⚙ Bash · rm -rf 漢字"), "{line}");
        assert!(line.ends_with('…'), "{line}");
        assert!(line.width() <= 24, "{} cells: {line}", line.width());
        // The ordinary line of the same call keeps the other end.
        let ordinary = tool_line(ToolState::Running, "bash", arguments, 24);
        assert!(ordinary.ends_with("tail of words"), "{ordinary}");
        // A command that fits is not marked, and a call with no argument is its label.
        let short = asked_tool_line(ToolState::Running, "bash", r#"{"command":"ls"}"#, 24);
        assert_eq!(short, "⚙ Bash · ls");
        assert_eq!(
            asked_tool_line(ToolState::Running, "bash", "null", 24),
            "⚙ Bash"
        );
    }

    /// A tool the interface has no phrasing for is shown whole, from its start, as its
    /// ordinary line shows it from its end.
    #[test]
    fn an_asked_line_for_an_unknown_tool_starts_at_the_start_of_its_arguments() {
        let arguments = r#"{"target":"production","force":true}"#;
        let line = asked_tool_line(ToolState::Running, "deploy", arguments, 30);
        assert!(line.starts_with("⚙ deploy · {"), "{line}");
        assert!(line.width() <= 30);
    }

    #[test]
    fn compact_is_the_default() {
        assert_eq!(Detail::default(), Detail::Compact);
    }

    #[test]
    fn a_tool_call_says_what_it_is_and_what_it_is_acting_on() {
        let line = tool_line(
            ToolState::Running,
            "read",
            r#"{"file_path":"src/view.rs","offset":1}"#,
            80,
        );
        assert_eq!(line, "⚙ Read File · src/view.rs");
        let line = tool_line(
            ToolState::Running,
            "edit",
            r#"{"file_path":"a.rs","old_string":"x"}"#,
            80,
        );
        assert_eq!(line, "⚙ Edit File · a.rs");
        let line = tool_line(
            ToolState::Running,
            "write",
            r#"{"file_path":"b.rs","content":"x"}"#,
            80,
        );
        assert_eq!(line, "⚙ Write File · b.rs");
        let line = tool_line(
            ToolState::Running,
            "bash",
            r#"{"command":"cargo test --workspace"}"#,
            80,
        );
        assert_eq!(line, "⚙ Bash · cargo test --workspace");
        let line = tool_line(
            ToolState::Running,
            "grep",
            r#"{"pattern":"follow","path":"src"}"#,
            80,
        );
        assert_eq!(line, "⚙ Grep · follow");
    }

    #[test]
    fn a_tool_the_interface_does_not_know_keeps_its_own_name() {
        // A guess would be worse than a raw name: the line would claim the call reads a
        // file when it does something else entirely.
        let line = tool_line(
            ToolState::Running,
            "fetch",
            r#"{"url":"https://example.com"}"#,
            80,
        );
        assert!(line.starts_with("⚙ fetch"), "{line}");
        assert!(line.contains("example.com"), "{line}");
    }

    #[test]
    fn a_call_with_no_arguments_is_the_tool_alone() {
        // `null` is what a frame from an agent that predates the arguments decodes to.
        assert_eq!(tool_line(ToolState::Running, "read", "", 80), "⚙ Read File");
        assert_eq!(
            tool_line(ToolState::Running, "read", "null", 80),
            "⚙ Read File"
        );
        assert_eq!(
            tool_line(ToolState::Running, "read", "{}", 80),
            "⚙ Read File"
        );
    }

    #[test]
    fn only_the_first_line_of_a_command_is_shown() {
        let line = tool_line(
            ToolState::Running,
            "bash",
            r#"{"command":"set -e\ncargo fmt"}"#,
            80,
        );
        assert_eq!(line, "⚙ Bash · set -e …");
    }

    #[test]
    fn a_long_line_keeps_its_label_and_the_end_of_what_it_acts_on() {
        let line = tool_line(
            ToolState::Running,
            "read",
            r#"{"file_path":"crates/nanus-bundle/src/tools/glob.rs"}"#,
            40,
        );
        assert_eq!(line.chars().count(), 40, "{line}");
        assert!(line.starts_with("⚙ Read File · …"), "{line}");
        assert!(line.ends_with("glob.rs"), "{line}");
    }

    /// The mark is the only place a call's outcome is recorded in this form, so it has to
    /// be the one the caller passed rather than a constant of this module's.
    #[test]
    fn a_tool_line_is_marked_with_the_outcome_it_was_given() {
        let arguments = r#"{"file_path":"a.rs"}"#;
        for state in [ToolState::Running, ToolState::Ok, ToolState::Failed] {
            let line = tool_line(state, "read", arguments, 80);
            assert!(
                line.starts_with(&format!("{} Read File", state.mark())),
                "{state:?} is drawn as {line}"
            );
            // And the label survives whichever mark it carries, on a narrow terminal too.
            let narrow = tool_line(state, "read", arguments, 20);
            assert!(
                narrow.starts_with(&format!("{} Read File", state.mark())),
                "{narrow}"
            );
        }
        assert_eq!(ToolState::Running.mark(), "⚙");
        assert_eq!(ToolState::Ok.mark(), "✓");
        assert_eq!(ToolState::Failed.mark(), "✗");
    }

    #[test]
    fn a_terminal_narrower_than_the_label_still_gets_a_line() {
        let line = tool_line(ToolState::Running, "read", r#"{"file_path":"a.rs"}"#, 4);
        assert_eq!(line.chars().count(), 4, "{line}");
    }

    #[test]
    fn a_thinking_line_is_the_newest_one() {
        let line = thinking_line("first thought\nsecond thought", false, 80);
        assert_eq!(line, "── thinking · second thought");
        assert!(!line.contains("first"), "{line}");
    }

    #[test]
    fn a_thinking_line_follows_the_end_of_a_paragraph() {
        // The newest line is what makes the line move as the model writes, so an appended
        // word changes what is drawn.
        let short = thinking_line("a very long thought that is still going", false, 80);
        let longer = thinking_line("a very long thought that is still going on", false, 80);
        assert_ne!(short, longer);
        assert!(longer.ends_with("on"), "{longer}");
    }

    #[test]
    fn an_empty_thinking_segment_is_just_its_label() {
        assert_eq!(thinking_line("", false, 80), "── thinking");
        assert_eq!(
            thinking_line("", true, 80),
            "── thinking▌",
            "and a cursor while it is still arriving"
        );
        assert_eq!(
            thinking_line("thought\n", true, 80),
            "── thinking▌",
            "a delta that ended with a newline is thinking into an empty line"
        );
    }

    #[test]
    fn a_streaming_line_stays_one_row_wide_with_its_cursor() {
        let line = thinking_line(&"x".repeat(200), true, 30);
        assert_eq!(line.chars().count(), 30, "{line}");
        assert!(line.ends_with('▌'), "{line}");
    }

    #[test]
    fn a_long_thinking_line_keeps_its_newest_words() {
        let line = thinking_line("aaaa bbbb cccc dddd eeee ffff", false, 20);
        assert_eq!(line.chars().count(), 20, "{line}");
        assert!(line.ends_with("ffff"), "{line}");
    }
}
