//! The `read` and `read_image` tools.
//!
//! Reading is the tool the model uses most, so its output shape matters more than
//! any other: it has to be bounded (a 50 MB file must not enter the transcript),
//! line-addressed (so the model can ask for a window), and explicit about how much
//! was *not* shown — a model that silently receives the first 2000 lines of a 5000
//! line file will confidently reason about a file it has not seen.
//!
//! ## Two kinds of window
//!
//! The line window (`offset`, `limit`) reads the whole file and numbers its lines, which is
//! the right shape for source code and is refused above [`MAX_READ_BYTES`]. The byte window
//! (`byte_offset`, `max_bytes`, `version`) reads one bounded range through
//! [`nanus_ports::FsPort::read_range`] and never the whole file, so a 50 MB log, a 5 MB
//! single-line JSON file, or a file that is not UTF-8 can still be looked at. The two are
//! mutually exclusive because a call that mixed them would have no single meaning; with no
//! byte argument the tool is exactly the line-window tool it always was.
//!
//! A byte window reports true byte offsets, the version (a token of the file's observed
//! identity) and the BLAKE3 of exactly the bytes shown. Passing the version back on the
//! next window makes the tool say whether the file changed in between — which is what a
//! model reading a large file piecewise needs to know, and which the tool states as a fact
//! about metadata, never as proof that the whole file is the same.

use core::fmt::Write as _;
use nanus_domain::context::managed::Digest;
use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{FileIdentity, FsHandle, RANGE_READ_MAX_BYTES, RangeRead};
use serde_json::{Value, json};

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

/// The number of bytes above which a file is reported as too large to read by lines.
pub const MAX_READ_BYTES: u64 = 4_194_304;

/// The default byte window, in bytes.
pub const DEFAULT_WINDOW_BYTES: u32 = 4_096;

/// The most text one byte window renders, frame included.
///
/// The port reads up to [`RANGE_READ_MAX_BYTES`]; this is the smaller bound on what reaches
/// the transcript. When the bytes read would render longer, the window shows a prefix and
/// its `next byte_offset` points at the first byte not shown, so nothing is skipped.
pub const MAX_WINDOW_RENDERED_BYTES: usize = 8_192;

/// Room kept for a window's header: two lines of fixed words, three numbers, the version,
/// the digest and the caller's earlier version, which is validated to a fixed length first.
const WINDOW_HEADER_RESERVE: usize = 512;

/// Room kept for the lines that follow a window's text: the invalid-UTF-8 count, the
/// continuation offset, and the clamp notice. Each is a fixed sentence with at most two
/// numbers, so the reserve is a bound rather than an estimate.
const WINDOW_FOOTER_RESERVE: usize = 256;

/// How many hex digits of the identity digest a version token carries.
const VERSION_DIGITS: usize = 16;

/// The arguments that select a byte window.
const BYTE_WINDOW_ARGUMENTS: [&str; 3] = ["byte_offset", "max_bytes", "version"];

/// The arguments that select a line window.
const LINE_WINDOW_ARGUMENTS: [&str; 2] = ["offset", "limit"];

/// Builds the `read` tool over `fs`.
pub fn read_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("read").unwrap_or_else(|_| unreachable!("read is a valid tool name")),
        description: "Read a text file from the workspace. Returns the file's contents with \
                      line numbers, and reports how many lines exist so you can request a \
                      window with `offset` and `limit`. For a file over 4 MiB, one huge \
                      line, or non-UTF-8 bytes, read a byte window with `byte_offset` and \
                      `max_bytes` instead; the two kinds of window cannot be combined."
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
                },
                "byte_offset": {
                    "type": "integer",
                    "description": "Byte window: 0-based byte to start at. Defaults to 0."
                },
                "max_bytes": {
                    "type": "integer",
                    "description": format!(
                        "Byte window: bytes to read. Defaults to {DEFAULT_WINDOW_BYTES}, at \
                         most {RANGE_READ_MAX_BYTES}; about 8 KiB is shown and the result \
                         gives the next byte_offset."
                    )
                },
                "version": {
                    "type": "string",
                    "description": "Byte window: the version an earlier window returned; the \
                                    result says whether the file changed since."
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, ReadExecutor { fs }).with_access(ToolAccess::Read)
}

/// Builds the `read_image` tool over `fs`.
pub fn read_image_tool(fs: FsHandle) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("read_image")
            .unwrap_or_else(|_| unreachable!("read_image is a valid tool name")),
        description: "Attach an image file to the conversation so you can see it. Supports \
                      bounded inline PNG and JPEG."
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
        .with_access(ToolAccess::Read)
        .with_result_images(
            1,
            u32::try_from(nanus_domain::content::IMAGE_BYTES_MAX).unwrap_or(u32::MAX),
        )
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
            match window_kind(&arguments) {
                Err(failure) => ToolResult::new(id, failure),
                Ok(WindowKind::Bytes) => byte_window_outcome(fs, id, &arguments).await,
                Ok(WindowKind::Lines) => read_outcome(fs, id, &arguments).await,
            }
        })
    }
}

/// Which kind of window a call asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowKind {
    /// Numbered lines of a whole-file read: the tool's original shape.
    Lines,
    /// One bounded byte range.
    Bytes,
}

/// Decides the window kind from which arguments are present.
///
/// A call with no byte-window argument is a line window, so every call written before byte
/// windows existed means what it meant. A `null` counts as absent, as it does for every
/// other optional argument.
fn window_kind(arguments: &Arguments<'_>) -> Result<WindowKind, ToolOutcome> {
    let present = |field: &&str| {
        arguments
            .raw()
            .get(field)
            .is_some_and(|value| !value.is_null())
    };
    let bytes = BYTE_WINDOW_ARGUMENTS.into_iter().find(present);
    let lines = LINE_WINDOW_ARGUMENTS.into_iter().find(present);
    match (bytes, lines) {
        (Some(byte_argument), Some(line_argument)) => Err(ToolOutcome::failure(format!(
            "read: {line_argument:?} selects a line window and {byte_argument:?} a byte \
             window; use offset/limit or byte_offset/max_bytes/version, not both"
        ))),
        (Some(_), None) => Ok(WindowKind::Bytes),
        (None, _) => Ok(WindowKind::Lines),
    }
}

/// Reads a file and renders it for the model.
async fn read_outcome(fs: FsHandle, id: ToolCallId, arguments: &Arguments<'_>) -> ToolResult {
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let offset = match arguments.optional_u32("offset") {
        Ok(offset) => offset.unwrap_or(1).max(1),
        Err(failure) => return ToolResult::new(id, failure),
    };
    let requested = match arguments.optional_u32("limit") {
        Ok(limit) => limit.unwrap_or(DEFAULT_READ_LIMIT),
        Err(failure) => return ToolResult::new(id, failure),
    };
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
    // Size is checked before the read so a huge file never enters memory. The refusal names
    // the byte window, because that is the route that does work for a file this size.
    if meta.byte_len > MAX_READ_BYTES {
        return ToolResult::new(
            id,
            ToolOutcome::failure(format!(
                "read: {path} is {} bytes, above the {MAX_READ_BYTES}-byte ceiling for a \
                 line window; read it in byte windows with byte_offset and max_bytes, or \
                 use grep to search it",
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

/// What a byte-window call asked for, after its arguments were checked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ByteRequest {
    /// The first byte asked for.
    offset: u64,
    /// The bytes to read, after the port's ceiling was applied.
    max_bytes: usize,
    /// Whether `max_bytes` was lowered to the port's ceiling.
    clamped: bool,
    /// The version an earlier window returned, when the call carried one.
    previous: Option<String>,
}

/// Checks a byte-window call's arguments.
///
/// A `max_bytes` above the port's ceiling is clamped and the result says so, for the reason
/// [`MAX_READ_LIMIT`] is clamped rather than refused; a zero is refused, for the reason a
/// zero `limit` is.
fn byte_request(arguments: &Arguments<'_>) -> Result<ByteRequest, ToolOutcome> {
    let offset = optional_u64(arguments, "byte_offset")?.unwrap_or(0);
    let requested = arguments
        .optional_u32("max_bytes")?
        .unwrap_or(DEFAULT_WINDOW_BYTES);
    if requested == 0 {
        return Err(ToolOutcome::failure("read: max_bytes must be at least 1"));
    }
    let requested = usize::try_from(requested).unwrap_or(usize::MAX);
    let max_bytes = requested.min(RANGE_READ_MAX_BYTES);
    let previous = arguments.optional_str("version")?;
    if let Some(previous) = previous.as_deref()
        && !is_version_token(previous)
    {
        return Err(ToolOutcome::failure(format!(
            "read: version must be the {VERSION_DIGITS}-digit version an earlier byte window \
             returned"
        )));
    }
    Ok(ByteRequest {
        offset,
        max_bytes,
        clamped: requested > max_bytes,
        previous,
    })
}

/// Reads an optional non-negative integer that may exceed `u32`, as a byte offset can.
fn optional_u64(arguments: &Arguments<'_>, field: &str) -> Result<Option<u64>, ToolOutcome> {
    match arguments.raw().get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ToolOutcome::failure(format!(
                "read needs {field:?} to be a non-negative integer, but it was not one"
            ))
        }),
    }
}

/// Reads one byte window and renders it for the model.
async fn byte_window_outcome(
    fs: FsHandle,
    id: ToolCallId,
    arguments: &Arguments<'_>,
) -> ToolResult {
    let path = match arguments.required_str("file_path") {
        Ok(path) => path,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let request = match byte_request(arguments) {
        Ok(request) => request,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let ranged = fs
        .read_range(
            std::path::Path::new(&path),
            request.offset,
            request.max_bytes,
        )
        .await;
    let read = match ranged {
        Ok(read) => read,
        Err(error) => return port_error_result(id, "read", &error),
    };
    let window = render_byte_window(&path, &read, &request);
    ToolResult::new(id, text_success(window.value, window.text))
}

/// A rendered byte window, and the facts it states in machine-readable form.
struct ByteWindow {
    text: String,
    value: Value,
}

/// The part of a window's bytes that fits the rendered budget, decoded for display.
#[derive(Debug, PartialEq, Eq)]
struct Decoded {
    /// The text shown, with U+FFFD for each invalid sequence.
    text: String,
    /// How many of the window's bytes that text accounts for.
    consumed: usize,
    /// How many invalid sequences were replaced.
    invalid: u32,
}

/// Renders a byte window: a header naming the range, version and digest, the text, and a
/// footer saying where to continue.
///
/// The bytes shown can be fewer than the bytes read, because the rendered budget is smaller
/// than the port's window. The range, the digest and the next offset all describe exactly
/// the bytes shown, so continuing from `next byte_offset` skips nothing.
fn render_byte_window(path: &str, read: &RangeRead, request: &ByteRequest) -> ByteWindow {
    // A port that returned more than it was asked for is held to the request.
    let read_bytes = read.bytes.get(..request.max_bytes).unwrap_or(&read.bytes);
    let reached_end = read.eof && read_bytes.len() == read.bytes.len();
    let budget = MAX_WINDOW_RENDERED_BYTES
        .saturating_sub(WINDOW_HEADER_RESERVE)
        .saturating_sub(WINDOW_FOOTER_RESERVE);
    let decoded = decode_bounded(read_bytes, budget, reached_end);
    let shown = read_bytes.get(..decoded.consumed).unwrap_or_default();
    let range_blake3 = if shown.len() == read.bytes.len() {
        read.range_blake3.clone()
    } else {
        Digest::of(shown)
    };
    let version = version_token(&read.identity);
    let facts = WindowFacts {
        start: read.offset,
        end: read
            .offset
            .saturating_add(u64::try_from(shown.len()).unwrap_or(u64::MAX)),
        len: read.identity.len,
        eof: reached_end && shown.len() == read_bytes.len(),
        same_as_previous: request
            .previous
            .as_deref()
            .map(|previous| previous == version),
        version,
        range_blake3,
    };
    let mut text = window_header(&facts, request);
    text.push_str(&decoded.text);
    if !decoded.text.is_empty() && !decoded.text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&window_footer(&facts, &decoded, request));
    // Postcondition: the frame's reserves are bounds, so the whole window fits the budget.
    assert!(
        text.len() <= MAX_WINDOW_RENDERED_BYTES,
        "a byte window renders within its budget"
    );
    let value = window_value(path, read, request, &facts, &decoded, read_bytes.len());
    ByteWindow { text, value }
}

/// The facts one window states, computed once so the text and the value cannot disagree.
struct WindowFacts {
    /// The first byte shown.
    start: u64,
    /// One past the last byte shown, which is where the next window starts.
    end: u64,
    /// The file's length when it was read.
    len: u64,
    /// Whether the bytes shown reach the end of the file.
    eof: bool,
    /// The token of the file's observed identity.
    version: String,
    /// BLAKE3 of exactly the bytes shown.
    range_blake3: Digest,
    /// Whether the identity matches the version the call passed, when it passed one.
    same_as_previous: Option<bool>,
}

/// Renders the header: the range, the version and the digest, then any change.
fn window_header(facts: &WindowFacts, request: &ByteRequest) -> String {
    let mut header = String::new();
    let _ = writeln!(
        header,
        "[bytes {}..{} of {}; version {}; blake3 {}]",
        facts.start, facts.end, facts.len, facts.version, facts.range_blake3
    );
    match (facts.same_as_previous, request.previous.as_deref()) {
        (Some(true), _) => {
            let _ = writeln!(
                header,
                "[unchanged since that version: same length, modification time and file id; \
                 this does not prove every byte is the same]"
            );
        }
        (Some(false), Some(previous)) => {
            let _ = writeln!(
                header,
                "[changed since version {previous}: the file's length, modification time or \
                 file id differ, so windows read before may not match this one]"
            );
        }
        _ => {}
    }
    assert!(
        header.len() <= WINDOW_HEADER_RESERVE,
        "the header fits its reserve"
    );
    header
}

/// Renders the footer: replaced sequences, where to continue, and any clamp.
fn window_footer(facts: &WindowFacts, decoded: &Decoded, request: &ByteRequest) -> String {
    let mut footer = String::new();
    if decoded.invalid > 0 {
        let _ = writeln!(
            footer,
            "[{} invalid UTF-8 sequences shown as U+FFFD]",
            decoded.invalid
        );
    }
    if facts.start > facts.len {
        let _ = writeln!(
            footer,
            "[byte_offset {} is past the end of the file, which has {} bytes]",
            facts.start, facts.len
        );
    } else if facts.eof {
        let _ = writeln!(footer, "[end of file]");
    } else {
        let _ = writeln!(
            footer,
            "[next byte_offset={}; {} bytes remain]",
            facts.end,
            facts.len.saturating_sub(facts.end)
        );
    }
    if request.clamped {
        let _ = writeln!(footer, "[max_bytes was lowered to {RANGE_READ_MAX_BYTES}]");
    }
    assert!(
        footer.len() <= WINDOW_FOOTER_RESERVE,
        "the footer fits its reserve"
    );
    footer
}

/// The machine-readable half of a window.
fn window_value(
    path: &str,
    read: &RangeRead,
    request: &ByteRequest,
    facts: &WindowFacts,
    decoded: &Decoded,
    read_len: usize,
) -> Value {
    json!({
        "file_path": path,
        "byte_offset": request.offset,
        "max_bytes": request.max_bytes,
        "max_bytes_clamped": request.clamped,
        "start": facts.start,
        "end": facts.end,
        "next_offset": facts.end,
        "returned_bytes": decoded.consumed,
        "read_bytes": read_len,
        "eof": facts.eof,
        "file_bytes": facts.len,
        "version": facts.version,
        "blake3": facts.range_blake3.as_str(),
        "invalid_utf8_sequences": decoded.invalid,
        "changed": facts.same_as_previous.map(|same| !same),
        "identity": identity_value(&read.identity),
    })
}

/// The observed identity, with the modification time as text because it is a `u128`.
fn identity_value(identity: &FileIdentity) -> Value {
    json!({
        "len": identity.len,
        "modified_ns": identity.modified_ns.map(|nanos| nanos.to_string()),
        "file_id": identity.file_id,
    })
}

/// A short token of a file's observed identity.
///
/// A digest of the identity rather than the identity itself, so the model carries one short
/// opaque word between calls instead of three fields it might reorder or round.
fn version_token(identity: &FileIdentity) -> String {
    let canonical = format!(
        "{}\n{:?}\n{:?}",
        identity.len, identity.modified_ns, identity.file_id
    );
    let digest = Digest::of(canonical.as_bytes());
    let token = digest
        .as_str()
        .get(..VERSION_DIGITS)
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        token.len(),
        VERSION_DIGITS,
        "a version token has a fixed length"
    );
    token
}

/// Whether `raw` has the shape of a version token.
fn is_version_token(raw: &str) -> bool {
    raw.len() == VERSION_DIGITS
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Decodes the longest prefix of `bytes` whose rendering fits `budget`.
///
/// Invalid UTF-8 is shown as U+FFFD and counted rather than refused: a byte window exists for
/// files the line window cannot read. A character cut in half by the end of a window that is
/// not the end of the file is not invalid, though — it is left for the next window, so the
/// next offset lands on its first byte rather than in its middle.
fn decode_bounded(bytes: &[u8], budget: usize, at_end: bool) -> Decoded {
    let mut decoded = Decoded {
        text: String::new(),
        consumed: 0,
        invalid: 0,
    };
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        let end = char_floor(valid, budget.saturating_sub(decoded.text.len()));
        decoded.text.push_str(valid.get(..end).unwrap_or_default());
        decoded.consumed = decoded.consumed.saturating_add(end);
        if end < valid.len() {
            break;
        }
        let invalid = chunk.invalid();
        if invalid.is_empty() {
            continue;
        }
        let tail = decoded.consumed.saturating_add(invalid.len()) == bytes.len();
        if tail && !at_end && decoded.consumed > 0 && is_incomplete(invalid) {
            break;
        }
        let replacement = char::REPLACEMENT_CHARACTER.len_utf8();
        if budget.saturating_sub(decoded.text.len()) < replacement {
            break;
        }
        decoded.text.push(char::REPLACEMENT_CHARACTER);
        decoded.consumed = decoded.consumed.saturating_add(invalid.len());
        decoded.invalid = decoded.invalid.saturating_add(1);
    }
    // Postconditions: the text fits, it accounts for a prefix of the bytes, and a window
    // with bytes to show shows at least one, so a reader always makes progress.
    assert!(decoded.text.len() <= budget, "decoded text fits its budget");
    assert!(
        decoded.consumed <= bytes.len(),
        "decoding consumes only the window"
    );
    assert!(
        bytes.is_empty() || budget < 4 || decoded.consumed > 0,
        "a window with bytes makes progress"
    );
    decoded
}

/// The largest character boundary of `text` at or below `at`.
fn char_floor(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let mut end = at;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}

/// Whether an invalid run is only the start of a character the window cut off.
fn is_incomplete(invalid: &[u8]) -> bool {
    std::str::from_utf8(invalid)
        .err()
        .is_some_and(|error| error.error_len().is_none())
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
                "read_image: {path} is not a supported image; use PNG or JPEG"
            )),
        );
    };

    // Size before reading, and check the bytes again afterwards in case the file grew.
    // Images have a stricter file bound than text; encoded size and media are checked
    // before the result can be retained or sent to a model.
    let metadata = fs.metadata(std::path::Path::new(&path)).await;
    let Ok(meta) = metadata else {
        let Err(error) = metadata else {
            unreachable!("a failed metadata call carries an error")
        };
        return port_error_result(id, "read_image", &error);
    };
    let ceiling = u64::try_from(nanus_domain::content::IMAGE_BYTES_MAX).unwrap_or(u64::MAX);
    if meta.byte_len > ceiling {
        return ToolResult::failure(
            id,
            format!("read_image: {path} exceeds the {ceiling}-byte ceiling"),
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
    if bytes.is_empty() || bytes.len() > nanus_domain::content::IMAGE_BYTES_MAX {
        return ToolResult::failure(id, "read_image: empty or oversized file after read");
    }
    let encoded = base64_encode(&bytes);
    if let Err(error) = nanus_domain::content::validate_image(media_type, &encoded) {
        return ToolResult::failure(id, error.to_string());
    }
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
    use nanus_domain::ToolCallId;
    use nanus_ports::FsPort;

    /// A filesystem handle for the argument paths, which never reaches the port.
    fn fs_handle() -> FsHandle {
        let port: Box<dyn FsPort> = Box::new(crate::tests_support::UnusedFs);
        std::rc::Rc::new(port)
    }

    /// A wrongly typed optional argument is refused, not folded into its default.
    #[tokio::test]
    async fn a_wrongly_typed_window_is_reported_rather_than_defaulted() {
        let tool = read_tool(fs_handle());
        for arguments in [
            json!({ "file_path": "a.txt", "offset": "first" }),
            json!({ "file_path": "a.txt", "limit": "all of it" }),
        ] {
            let result = tool
                .execute(ToolCall::new(
                    ToolCallId::new("c1"),
                    ToolName::new("read").unwrap_or_else(|_| unreachable!("read is valid")),
                    arguments.clone(),
                ))
                .await;
            let ToolOutcome::Failure { message, .. } = &result.outcome else {
                panic!("{arguments} must be refused: {:?}", result.outcome);
            };
            assert!(
                message.contains("offset") || message.contains("limit"),
                "{message}"
            );
            assert!(
                !message.contains("does not exist"),
                "the call is refused before any file is looked for: {message}"
            );
        }
    }

    use crate::tests_support::MemoryFs;

    /// Runs `read` with `arguments` over `fs`.
    async fn read(fs: &MemoryFs, arguments: Value) -> ToolResult {
        read_tool(fs.handle())
            .execute(ToolCall::new(
                ToolCallId::new("c-read"),
                ToolName::new("read").unwrap_or_else(|_| unreachable!("read is valid")),
                arguments,
            ))
            .await
    }

    /// The text and value of a successful result.
    fn success(result: &ToolResult) -> (String, Value) {
        assert!(result.outcome.is_success(), "{:?}", result.outcome);
        let value = result.outcome.value().cloned().unwrap_or_default();
        (result.outcome.render_text(), value)
    }

    /// The legacy shape, pinned byte for byte: a call with no byte-window argument must not
    /// notice that byte windows exist.
    #[tokio::test]
    async fn a_call_without_byte_arguments_is_the_line_window_it_always_was() {
        let fs = MemoryFs::new("one\ntwo\nthree");
        let (text, value) = success(&read(&fs, json!({ "file_path": "f.txt" })).await);
        assert_eq!(
            text,
            "1: one\n2: two\n3: three\n(end of file: 3 lines total)\n"
        );
        assert_eq!(
            value,
            json!({
                "file_path": "f.txt",
                "offset": 1,
                "limit": DEFAULT_READ_LIMIT,
                "total_lines": 3,
                "returned_lines": 3,
            })
        );
        let args = json!({ "file_path": "f.txt", "offset": 2, "limit": 1, "version": null });
        let (text, _) = success(&read(&fs, args).await);
        assert_eq!(
            text,
            "2: two\n(showing lines 2-2 of 3; use offset=3 to continue)\n"
        );
    }

    #[tokio::test]
    async fn line_and_byte_window_arguments_cannot_be_combined() {
        let fs = MemoryFs::new("one\ntwo\n");
        for arguments in [
            json!({ "file_path": "f.txt", "offset": 1, "byte_offset": 0 }),
            json!({ "file_path": "f.txt", "limit": 5, "max_bytes": 10 }),
            json!({ "file_path": "f.txt", "offset": 1, "version": "0123456789abcdef" }),
        ] {
            let result = read(&fs, arguments.clone()).await;
            let Some(message) = result.outcome.message() else {
                panic!("{arguments} must be refused");
            };
            assert!(message.contains("not both"), "{message}");
        }
        // Each kind on its own is accepted.
        let (text, _) = success(&read(&fs, json!({ "file_path": "f.txt", "max_bytes": 3 })).await);
        assert!(text.contains("one"), "{text}");
        assert!(text.contains("[bytes 0..3 of 8;"), "{text}");
    }

    #[tokio::test]
    async fn a_file_over_four_mebibytes_is_refused_by_lines_and_read_by_bytes() {
        let contents: Vec<u8> = (0..5_000_000_u32)
            .map(|index| b"abcdefghij\n"[usize::try_from(index % 11).unwrap_or(0)])
            .collect();
        let fs = MemoryFs::new(contents.clone());
        let refused = read(&fs, json!({ "file_path": "big.log" })).await;
        let message = refused.outcome.message().unwrap_or_default().to_owned();
        assert!(message.contains("ceiling"), "{message}");
        assert!(
            message.contains("byte_offset"),
            "the refusal names the route: {message}"
        );

        let mut offset = 4_500_000_u64;
        let mut seen = 0_usize;
        for _ in 0..3 {
            let arguments = json!({ "file_path": "big.log", "byte_offset": offset });
            let (text, value) = success(&read(&fs, arguments).await);
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let returned =
                usize::try_from(value["returned_bytes"].as_u64().unwrap_or(0)).unwrap_or(0);
            let slice = &contents[start..start + returned];
            assert_eq!(value["start"], offset);
            assert_eq!(value["blake3"], Digest::of(slice).as_str());
            assert!(
                text.contains(std::str::from_utf8(slice).unwrap_or("?")),
                "{text}"
            );
            assert_eq!(
                value["next_offset"],
                offset + u64::try_from(returned).unwrap_or(0)
            );
            offset = value["next_offset"].as_u64().unwrap_or(0);
            seen += returned;
        }
        assert_eq!(seen, 3 * usize::try_from(DEFAULT_WINDOW_BYTES).unwrap_or(0));
    }

    #[tokio::test]
    async fn a_huge_single_line_renders_within_the_budget_and_a_small_window_is_whole() {
        let fs = MemoryFs::new("x".repeat(5 * 1024 * 1024));
        let arguments = json!({ "file_path": "line.json", "max_bytes": 65_536 });
        let (text, value) = success(&read(&fs, arguments).await);
        assert!(text.len() <= MAX_WINDOW_RENDERED_BYTES, "{}", text.len());
        let returned = value["returned_bytes"].as_u64().unwrap_or(0);
        assert!(returned > 4_096 && returned < 65_536, "{returned}");
        assert_eq!(
            value["read_bytes"], 65_536,
            "the port read the whole window"
        );
        assert_eq!(value["next_offset"], returned, "continuing skips nothing");
        assert_eq!(value["eof"], false);
        assert!(
            text.contains(&format!("[next byte_offset={returned};")),
            "{text}"
        );

        // The other direction: a window that fits is shown whole, and a too-large request is
        // clamped and says so.
        let arguments = json!({ "file_path": "line.json", "max_bytes": 100 });
        let (_, value) = success(&read(&fs, arguments).await);
        assert_eq!(value["returned_bytes"], 100);
        let arguments = json!({ "file_path": "line.json", "max_bytes": 1_000_000 });
        let (text, value) = success(&read(&fs, arguments).await);
        assert_eq!(value["max_bytes_clamped"], true);
        assert!(text.contains("max_bytes was lowered to 65536"), "{text}");
    }

    #[tokio::test]
    async fn a_file_changed_between_windows_is_reported_and_an_unchanged_one_is_not() {
        let fs = MemoryFs::new("first version of the file\n");
        let arguments = json!({ "file_path": "f.txt", "max_bytes": 5 });
        let (_, first) = success(&read(&fs, arguments).await);
        let version = first["version"].as_str().unwrap_or_default().to_owned();
        assert_eq!(
            first["changed"],
            Value::Null,
            "no version given, no claim made"
        );

        let arguments = json!({ "file_path": "f.txt", "byte_offset": 5, "version": version });
        let (text, value) = success(&read(&fs, arguments).await);
        assert_eq!(value["changed"], false);
        assert!(text.contains("unchanged since that version"), "{text}");
        assert!(
            text.contains("does not prove"),
            "metadata is not a snapshot: {text}"
        );

        fs.replace("a rewritten file\n");
        let arguments = json!({ "file_path": "f.txt", "byte_offset": 5, "version": version });
        let (text, value) = success(&read(&fs, arguments).await);
        assert_eq!(value["changed"], true);
        assert!(
            text.contains(&format!("changed since version {version}")),
            "{text}"
        );
        assert_ne!(value["version"], first["version"]);
    }

    #[tokio::test]
    async fn invalid_utf8_is_shown_replaced_and_counted() {
        let fs = MemoryFs::new(b"ok\xff\xfeok".to_vec());
        let (text, value) =
            success(&read(&fs, json!({ "file_path": "f.bin", "byte_offset": 0 })).await);
        assert!(text.contains("ok\u{FFFD}"), "{text}");
        assert_eq!(value["invalid_utf8_sequences"], 2);
        assert!(text.contains("2 invalid UTF-8 sequences"), "{text}");
        assert!(text.contains("[end of file]"), "{text}");

        let clean = MemoryFs::new("all valid");
        let (text, value) =
            success(&read(&clean, json!({ "file_path": "f", "byte_offset": 0 })).await);
        assert_eq!(value["invalid_utf8_sequences"], 0);
        assert!(!text.contains("invalid"), "{text}");
    }

    #[tokio::test]
    async fn a_character_cut_by_the_window_is_left_for_the_next_one() {
        let fs = MemoryFs::new("aé");
        let arguments = json!({ "file_path": "f", "byte_offset": 0, "max_bytes": 2 });
        let (_, value) = success(&read(&fs, arguments).await);
        assert_eq!(
            value["returned_bytes"], 1,
            "the half character is not shown"
        );
        assert_eq!(value["next_offset"], 1);
        assert_eq!(value["invalid_utf8_sequences"], 0);
        // The next window starts on the character and shows it whole.
        let arguments = json!({ "file_path": "f", "byte_offset": 1 });
        let (text, value) = success(&read(&fs, arguments).await);
        assert!(text.contains('é'), "{text}");
        assert_eq!(value["eof"], true);
        // Starting inside the character is the caller's choice, and is reported as invalid.
        let arguments = json!({ "file_path": "f", "byte_offset": 2 });
        let (_, value) = success(&read(&fs, arguments).await);
        assert_eq!(value["invalid_utf8_sequences"], 1);
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_reported_and_one_at_the_end_is_the_end() {
        let fs = MemoryFs::new("abc");
        let arguments = json!({ "file_path": "f", "byte_offset": 5_000_000_000_u64 });
        let (text, value) = success(&read(&fs, arguments).await);
        assert!(
            text.contains("past the end of the file, which has 3 bytes"),
            "{text}"
        );
        assert_eq!(value["returned_bytes"], 0);
        assert_eq!(value["eof"], true);
        let (text, _) = success(&read(&fs, json!({ "file_path": "f", "byte_offset": 3 })).await);
        assert!(text.contains("[end of file]"), "{text}");
        assert!(!text.contains("past the end"), "{text}");
    }

    #[tokio::test]
    async fn malformed_byte_window_arguments_are_refused_before_any_read() {
        let fs = MemoryFs::new("abc");
        for (arguments, expected) in [
            (json!({ "file_path": "f", "max_bytes": 0 }), "at least 1"),
            (
                json!({ "file_path": "f", "byte_offset": -1 }),
                "byte_offset",
            ),
            (
                json!({ "file_path": "f", "byte_offset": "0" }),
                "byte_offset",
            ),
            (
                json!({ "file_path": "f", "version": "not-a-version" }),
                "version",
            ),
            (
                json!({ "file_path": "f", "version": "0123456789ABCDEF" }),
                "version",
            ),
        ] {
            let result = read(&fs, arguments.clone()).await;
            let message = result.outcome.message().unwrap_or_default();
            assert!(message.contains(expected), "{arguments}: {message}");
        }
    }

    #[test]
    fn a_version_follows_the_identity_and_only_the_identity() {
        let identity = FileIdentity {
            len: 10,
            modified_ns: Some(7),
            file_id: Some(String::from("1:2")),
        };
        let token = version_token(&identity);
        assert!(is_version_token(&token), "{token}");
        let same = FileIdentity {
            len: 10,
            modified_ns: Some(7),
            file_id: Some(String::from("1:2")),
        };
        assert_eq!(
            token,
            version_token(&same),
            "an equal identity has the same version"
        );
        for changed in [
            FileIdentity {
                len: 11,
                ..identity.clone()
            },
            FileIdentity {
                modified_ns: Some(8),
                ..identity.clone()
            },
            FileIdentity {
                file_id: None,
                ..identity
            },
        ] {
            assert_ne!(version_token(&changed), token, "{changed:?}");
        }
    }

    #[test]
    fn decoding_stops_at_the_budget_on_a_character_boundary() {
        let text = "é".repeat(100);
        let decoded = decode_bounded(text.as_bytes(), 11, false);
        assert_eq!(decoded.text, "é".repeat(5));
        assert_eq!(decoded.consumed, 10);
        let whole = decode_bounded(text.as_bytes(), 1_000, true);
        assert_eq!(whole.consumed, 200);
        assert_eq!(whole.invalid, 0);
        // A truncated character at the true end of the file is invalid, not deferred.
        let tail = decode_bounded(&[b'a', 0xC3], 100, true);
        assert_eq!((tail.consumed, tail.invalid), (2, 1));
        let deferred = decode_bounded(&[b'a', 0xC3], 100, false);
        assert_eq!((deferred.consumed, deferred.invalid), (1, 0));
    }

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
