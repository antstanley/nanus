//! The `read` and `read_image` tools.
//!
//! Reading is the tool the model uses most, so its output shape matters more than
//! any other: it has to be bounded (a 50 MB file must not enter the transcript),
//! line-addressed (so the model can ask for a window), and explicit about how much
//! was *not* shown — a model that silently receives the first 2000 lines of a 5000
//! line file will confidently reason about a file it has not seen.

use core::fmt::Write as _;
use nanus_domain::{
    ContentBlock, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::FsHandle;
use serde_json::json;

use crate::args::Arguments;
use crate::tools::{image_media_type, port_error_result, text_success};

/// The default window size, in lines.
pub const DEFAULT_READ_LIMIT: u32 = 2_000;

/// The largest window a caller may request.
///
/// A larger window is not refused outright — it is clamped — because the model has
/// no reliable way to know the limit, and a refused read teaches it less than a
/// clamped one that says so.
pub const MAX_READ_LIMIT: u32 = 20_000;

/// The number of bytes above which a file is reported as too large to read.
pub const MAX_READ_BYTES: u64 = 4_194_304;

/// Builds the `read` tool over `fs`.
pub fn read_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("read").unwrap_or_else(|_| unreachable!("read is a valid tool name")),
        description: "Read a text file from the workspace. Returns the file's contents with \
                      line numbers, and reports how many lines exist so you can request a \
                      window with `offset` and `limit`."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace root."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line to start at. Defaults to 1."
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "How many lines to return. Defaults to {DEFAULT_READ_LIMIT}."
                    )
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, ReadExecutor { fs })
}

/// Builds the `read_image` tool over `fs`.
pub fn read_image_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("read_image")
            .unwrap_or_else(|_| unreachable!("read_image is a valid tool name")),
        description: "Attach an image file to the conversation so you can see it. Supports \
                      PNG, JPEG, WebP, and GIF."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the image, relative to the workspace root."
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, ReadImageExecutor { fs })
}

/// Executes `read`.
struct ReadExecutor {
    fs: FsHandle,
}

impl ToolExecutor for ReadExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        // The handle is cloned before the future is built, so the future borrows
        // nothing from the registry that dispatched it.
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move {
            let id = call.id.clone();
            let arguments = Arguments::new("read", &call.arguments);

            read_outcome(fs, id, &arguments).await
        })
    }
}

/// Reads a file and renders it for the model.
async fn read_outcome(fs: FsHandle, id: ToolCallId, arguments: &Arguments<'_>) -> ToolResult {
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let offset = arguments
        .optional_u32("offset")
        .unwrap_or(None)
        .unwrap_or(1)
        .max(1);
    let requested = arguments
        .optional_u32("limit")
        .unwrap_or(None)
        .unwrap_or(DEFAULT_READ_LIMIT);
    // A zero-line window is nonsense, and the workspace's habit is to refuse a nonsense
    // budget rather than reinterpret it: `AgentConfig::validate` rejects a zero step budget
    // for the same reason. Reinterpreting it as one would answer a question the model did
    // not ask, and passing it on used to abort the process on `render_window`'s
    // precondition.
    if requested == 0 {
        return ToolResult::new(
            id,
            ToolOutcome::failure(String::from("read: limit must be at least 1")),
        );
    }

    let metadata = fs.metadata(std::path::Path::new(&path)).await;
    let Ok(meta) = metadata else {
        let Err(error) = metadata else {
            unreachable!("a failed metadata call carries an error")
        };
        return port_error_result(id, "read", &error);
    };
    // Size is checked before the read so a huge file never enters memory.
    if meta.byte_len > MAX_READ_BYTES {
        return ToolResult::new(
            id,
            ToolOutcome::failure(format!(
                "read: {path} is {} bytes, above the {MAX_READ_BYTES}-byte ceiling; \
                 use grep to search it instead",
                meta.byte_len
            )),
        );
    }

    let read = fs.read(std::path::Path::new(&path)).await;
    let file = match read {
        Ok(file) => file,
        Err(error) => return port_error_result(id, "read", &error),
    };

    let limit = requested.min(MAX_READ_LIMIT);
    let rendered = render_window(&file.text, &path, offset, limit);
    let value = json!({
        "file_path": path,
        "offset": offset,
        "limit": limit,
        "total_lines": file.total_lines,
        "returned_lines": rendered.returned,
    });
    ToolResult::new(id, text_success(value, rendered.text))
}

/// A rendered window of a file, with the count of lines it contains.
struct Window {
    text: String,
    returned: u32,
}

/// Renders `offset`/`limit` lines of `text` with line numbers.
///
/// The footer is the important part: it states both how many lines were shown and
/// how many exist, because a model that cannot tell a complete file from a window
/// will reason about the wrong thing.
fn render_window(text: &str, _path: &str, offset: u32, limit: u32) -> Window {
    // Precondition: callers pass a 1-based offset and a non-zero limit.
    assert!(offset >= 1, "the first line is line 1");
    assert!(limit >= 1, "a window shows at least one line");

    let start = usize::try_from(offset.saturating_sub(1)).unwrap_or(usize::MAX);
    let take = usize::try_from(limit).unwrap_or(usize::MAX);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let end = start.saturating_add(take).min(total);

    let mut rendered = String::new();
    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        // Line numbers are 1-based, which is what every editor and every `grep`
        // output the model has seen uses.
        let number = index.saturating_add(1);
        let _ = writeln!(rendered, "{number}: {line}");
    }
    let returned = end.saturating_sub(start);
    let returned = u32::try_from(returned).unwrap_or(u32::MAX);

    if returned == 0 {
        let _ = writeln!(
            rendered,
            "(no lines: the file has {total} lines and offset {offset} is past the end)"
        );
    } else if end >= total {
        let _ = writeln!(rendered, "(end of file: {total} lines total)");
    } else {
        let _ = writeln!(
            rendered,
            "(showing lines {offset}-{end} of {total}; use offset={} to continue)",
            end.saturating_add(1)
        );
    }
    Window {
        text: rendered,
        returned,
    }
}

/// Executes `read_image`.
struct ReadImageExecutor {
    fs: FsHandle,
}

impl ToolExecutor for ReadImageExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let fs = std::rc::Rc::clone(&self.fs);
        Box::pin(async move { read_image_outcome(fs, call).await })
    }
}

/// Reads an image and renders it as a content block.
async fn read_image_outcome(fs: FsHandle, call: ToolCall) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("read_image", &call.arguments);
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let Some(media_type) = image_media_type(std::path::Path::new(&path)) else {
        return ToolResult::new(
            id,
            ToolOutcome::failure(format!(
                "read_image: {path} is not a supported image; use PNG, JPEG, WebP, or GIF"
            )),
        );
    };

    // Size first, as `read` does, so a huge image never enters memory. The ceiling is the
    // same one: both tools put a file's contents into the model's context, and base64 makes
    // an image a third larger again than the bytes it came from.
    let metadata = fs.metadata(std::path::Path::new(&path)).await;
    let Ok(meta) = metadata else {
        let Err(error) = metadata else {
            unreachable!("a failed metadata call carries an error")
        };
        return port_error_result(id, "read_image", &error);
    };
    if meta.byte_len > MAX_READ_BYTES {
        return ToolResult::new(
            id,
            ToolOutcome::failure(format!(
                "read_image: {path} is {} bytes, above the {MAX_READ_BYTES}-byte ceiling",
                meta.byte_len
            )),
        );
    }

    // Read through the port, which is what confines the path to the workspace root. This
    // tool used to read the raw model string with `tokio::fs`, which meant any readable
    // image anywhere on the host — and a relative path resolved against the process's
    // working directory rather than the workspace this tool's own schema promises.
    let bytes = match fs.read_bytes(std::path::Path::new(&path)).await {
        Ok(bytes) => bytes,
        Err(error) => return port_error_result(id, "read_image", &error),
    };
    if bytes.is_empty() {
        return ToolResult::new(
            id,
            ToolOutcome::failure(format!("read_image: {path} is empty")),
        );
    }
    let encoded = base64_encode(&bytes);
    let outcome = ToolOutcome::success_with(
        json!({ "file_path": path, "media_type": media_type, "bytes": bytes.len() }),
        vec![
            ContentBlock::Text(format!("{path} ({media_type}, {} bytes)", bytes.len())),
            ContentBlock::Image {
                media_type: media_type.to_owned(),
                data_base64: encoded,
            },
        ],
    );
    ToolResult::new(id, outcome)
}

/// Encodes bytes as standard base64.
///
/// Written out rather than depended upon: the alphabet and padding are fixed by
/// RFC 4648, the implementation has no branch that can fail, and it keeps the
/// adapter's dependency set to what the harness actually reasons about.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // Precondition: the alphabet is exactly 64 symbols, which is what makes six bits
    // address it without a bounds check at every use.
    assert_eq!(ALPHABET.len(), 64, "base64 has 64 symbols");

    let mut encoded = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0);
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        let symbols = [
            (triple >> 18) & 0x3f,
            (triple >> 12) & 0x3f,
            (triple >> 6) & 0x3f,
            triple & 0x3f,
        ];
        for (position, symbol) in symbols.iter().enumerate() {
            // A chunk shorter than three bytes has fewer than four symbols; the
            // surplus positions become padding.
            let short = position > chunk.len();
            let index = usize::try_from(*symbol).unwrap_or(0).min(63);
            let symbol = if short {
                b'='
            } else {
                ALPHABET.get(index).copied().unwrap_or(b'=')
            };
            encoded.push(char::from(symbol));
        }
    }
    // Postcondition: every encoded length is a multiple of four, which is what makes
    // the output decodable without a length prefix.
    assert_eq!(
        encoded.len() % 4,
        0,
        "base64 output is padded to a multiple of four"
    );
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // The RFC 4648 test vectors, which pin the alphabet and the padding.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_pads_every_length_to_a_multiple_of_four() {
        for length in 0..40_usize {
            let bytes = vec![0xAB_u8; length];
            let encoded = base64_encode(&bytes);
            assert_eq!(encoded.len() % 4, 0, "length {length}");
        }
    }

    #[test]
    fn base64_handles_the_full_byte_range() {
        // Every byte value must encode without a lookup failure, which is what the
        // alphabet assertion guards.
        let bytes: Vec<u8> = (0..=u8::MAX).collect();
        let encoded = base64_encode(&bytes);
        assert!(!encoded.is_empty());
        assert!(encoded.is_ascii());
        assert_eq!(encoded.len() % 4, 0);
    }

    #[test]
    fn a_window_reports_the_lines_it_showed() {
        let text = "one\ntwo\nthree\nfour\nfive";
        let window = render_window(text, "f.txt", 1, 2);
        assert_eq!(window.returned, 2);
        assert!(window.text.contains("1: one"));
        assert!(window.text.contains("2: two"));
        // The model must be able to tell a window from a whole file.
        assert!(
            window.text.contains("showing lines 1-2 of 5"),
            "{}",
            window.text
        );
        assert!(window.text.contains("offset=3"), "{}", window.text);
    }

    #[test]
    fn a_window_reaching_the_end_says_so() {
        let text = "one\ntwo";
        let window = render_window(text, "f.txt", 1, 10);
        assert_eq!(window.returned, 2);
        assert!(
            window.text.contains("end of file: 2 lines total"),
            "{}",
            window.text
        );
    }

    #[test]
    fn an_offset_past_the_end_is_reported_not_silent() {
        let text = "one\ntwo";
        let window = render_window(text, "f.txt", 99, 10);
        assert_eq!(window.returned, 0);
        // Silence here would look like an empty file.
        assert!(window.text.contains("past the end"), "{}", window.text);
        assert!(window.text.contains("2 lines"), "{}", window.text);
    }

    #[test]
    fn a_window_is_line_addressed_from_one() {
        let text = "alpha\nbeta\ngamma";
        let window = render_window(text, "f.txt", 2, 1);
        assert_eq!(window.returned, 1);
        assert!(window.text.starts_with("2: beta"), "{}", window.text);
    }

    #[test]
    fn an_empty_file_renders_a_complete_window() {
        let window = render_window("", "f.txt", 1, 10);
        // An empty file has zero lines, and the wording must not suggest there is
        // more to read.
        assert_eq!(window.returned, 0);
        assert!(window.text.contains("0 lines") || window.text.contains("past the end"));
    }
}
