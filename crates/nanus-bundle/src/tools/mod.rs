//! The model-facing toolset.
//!
//! Seven tools, and the count is the design.
//!
//! The harness exposes *mechanisms*, not conveniences:
//!
//! | Tool | What it is |
//! |---|---|
//! | [`read_tool`] | Read a file, with a line window and a line count. |
//! | [`write_tool`] | Create or replace a file. |
//! | [`edit_tool`] | Replace text in a file, exactly once by default. |
//! | [`read_image_tool`] | Attach an image to the conversation. |
//! | [`glob_tool`] | Find files by name. |
//! | [`grep_tool`] | Find text inside files. |
//! | [`bash_tool`] | Run a program. |
//!
//! There is deliberately no `list_directory`, no `move_file`, no `make_directory`,
//! and no per-language tool: a shell and a filesystem already cover those, and every
//! additional tool is another description in every request, another schema for the
//! model to choose between, and another surface to secure. A tool earns its place by
//! being something the shell *cannot* do — bounded output, a read-before-edit
//! guarantee, a diff — not by being convenient.
//!
//! ## Two halves, and the allowlist between them
//!
//! Each factory returns a [`ToolDefinition`], which pairs a wire [`ToolSchema`] with
//! a boxed executor. Only the schema half can be serialised, so nothing an executor
//! closes over — a filesystem root, a sandbox policy, a session id — can reach a
//! model request. That property is carried by the type rather than by review.
//!
//! ## Arguments are validated, never assumed
//!
//! Every tool reads its arguments through [`crate::args::Arguments`], so a
//! model that sends `{"limit": "ten"}` gets a specific correction instead of a
//! panic, and a failure is a model-visible [`nanus_domain::ToolOutcome::Failure`]
//! rather than a harness error.

mod bash;
mod edit;
mod glob;
mod grep;
mod read;
mod write;

pub use bash::bash_tool;
pub use edit::edit_tool;
pub use glob::glob_tool;
pub use grep::grep_tool;
pub use read::{read_image_tool, read_tool};
pub use write::write_tool;

use nanus_domain::{ContentBlock, ToolOutcome};

/// Renders a port failure as a model-facing outcome.
///
/// A port error is not a harness failure: "the file does not exist" is information
/// the model needs in order to try something else. Only the rendered message crosses
/// the boundary, so the core never learns what a `std::io::Error` or a
/// `reqwest::Error` is.
///
/// Generic over the error rather than over the port, because each port has its own
/// error type and all a tool needs is something renderable.
#[must_use]
pub fn port_failure(tool: &str, error: &impl core::fmt::Display) -> ToolOutcome {
    // Postcondition: the message names the tool, so a transcript with several failed
    // calls in a row still reads unambiguously.
    assert!(!tool.is_empty(), "a failure names the tool");
    ToolOutcome::failure(format!("{tool}: {error}"))
}

/// Turns a failed port call into the tool result that answers it.
///
/// The port's own error type is translated here and nowhere else, which is the
/// boundary rule: the domain and the kernel see only strings.
#[must_use]
pub fn port_error_result(
    call_id: nanus_domain::ToolCallId,
    tool: &str,
    error: &impl core::fmt::Display,
) -> nanus_domain::ToolResult {
    nanus_domain::ToolResult::new(call_id, port_failure(tool, error))
}

/// Renders a successful outcome whose content is a single text block.
#[must_use]
pub fn text_success(value: serde_json::Value, text: impl Into<String>) -> ToolOutcome {
    ToolOutcome::success_with(value, vec![ContentBlock::Text(text.into())])
}

/// Renders a successful outcome with no model-visible content.
///
/// Used where the value alone is the answer and a rendering would only restate it.
#[must_use]
pub fn silent_success(value: serde_json::Value) -> ToolOutcome {
    ToolOutcome::success(value)
}

/// Returns `true` when a path looks like a raster image the model may be shown.
///
/// The check is by extension because the content type is what a provider needs, and
/// sniffing magic bytes would mean reading a file the caller may not have permission
/// to read. An unsupported extension is reported rather than guessed at.
#[must_use]
pub fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn image_extensions_map_to_media_types() {
        // Positive space: every supported extension.
        assert_eq!(image_media_type(Path::new("a.png")), Some("image/png"));
        assert_eq!(image_media_type(Path::new("a.jpg")), Some("image/jpeg"));
        assert_eq!(image_media_type(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(image_media_type(Path::new("a.webp")), Some("image/webp"));
        assert_eq!(image_media_type(Path::new("a.gif")), Some("image/gif"));
        // The extension is matched case-insensitively, because a screenshot tool
        // commonly writes `.PNG`.
        assert_eq!(image_media_type(Path::new("a.PNG")), Some("image/png"));
    }

    #[test]
    fn unsupported_extensions_are_rejected_rather_than_guessed() {
        // Negative space: guessing a content type would send the provider bytes it
        // cannot decode.
        assert_eq!(image_media_type(Path::new("a.svg")), None);
        assert_eq!(image_media_type(Path::new("a.bmp")), None);
        assert_eq!(image_media_type(Path::new("a.txt")), None);
        assert_eq!(image_media_type(Path::new("noext")), None);
        assert_eq!(image_media_type(Path::new("a")), None);
    }

    #[test]
    fn a_port_failure_names_the_tool_and_the_reason() {
        let error = nanus_ports::FsError::NotFound {
            path: std::path::PathBuf::from("/tmp/missing.txt"),
        };
        let outcome = port_failure("read", &error);
        let ToolOutcome::Failure { message, content } = &outcome else {
            panic!("a port failure is a model-facing failure");
        };
        assert!(message.starts_with("read:"), "{message}");
        assert!(message.contains("missing.txt"), "{message}");
        // The content carries the same text, because that is what the model reads.
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn a_text_success_carries_one_block() {
        let outcome = text_success(serde_json::json!({ "ok": true }), "done");
        assert!(outcome.is_success());
        assert_eq!(outcome.content().len(), 1);
        assert!(
            matches!(outcome.content().first(), Some(ContentBlock::Text(text)) if text == "done")
        );
    }

    #[test]
    fn a_silent_success_carries_no_content() {
        let outcome = silent_success(serde_json::json!({ "ok": true }));
        assert!(outcome.is_success());
        assert!(outcome.content().is_empty());
    }
}
