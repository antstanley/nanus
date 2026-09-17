//! Putting selected text where the reader can use it.
//!
//! ## Why the platform's own tools
//!
//! A terminal has no clipboard API: setting one is a platform call, and the crates that make it pull
//! in a windowing system this program never opens. `pbcopy` ships with macOS, and `wl-copy` or
//! `xclip` is what a Wayland or X session provides, so a copied selection is handed to one of those
//! — the same reasoning as reading the clipboard for [`crate::paste`], one direction over.
//!
//! ## Why the terminal is the fallback
//!
//! On a machine with none of those — a server reached over ssh, a session with no X — there is still
//! a clipboard: the terminal's own, reachable with `OSC 52`, which asks the terminal emulator to take
//! a base64 payload. That needs no tool and no window system, and it is the only path that works
//! where this program is most likely to be run over a link.
//!
//! Nothing here can *confirm* the terminal took it: `OSC 52` is a request with no reply, and a
//! terminal that does not implement it ignores it silently. That is why the two destinations are
//! reported differently — see [`Copied`] — rather than both being called "copied".
//!
//! ## Why this is asynchronous
//!
//! The tool is a child process, and it is *awaited* rather than waited on. This runs from the
//! interface's event loop, and that loop is the thread the link's transport is also read on: a child
//! waited on synchronously would stop a running turn's frames arriving for as long as the tool took,
//! and would freeze the interface outright if the tool never finished — which is exactly the argument
//! `crate::shell` makes for the `!` escape it runs.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says which surface these items are meant for, and this is the lint's counterpart —
// the same allow `paste.rs`, `shell.rs`, `mentions.rs`, `help.rs`, and `markdown/mod.rs` carry.
#![allow(clippy::redundant_pub_crate)]

// Both: the clipboard tool's input is written through tokio's pipe, and the `OSC 52` escape goes to
// this process's own standard output, which is an ordinary blocking handle.
use std::io::Write as _;
use std::process::Stdio;

use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

/// Where a copy ended up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Copied {
    /// The platform's clipboard tool took it, which is the ordinary system clipboard.
    System,
    /// The terminal was asked for its clipboard. Nothing can confirm it listened.
    Terminal,
}

/// Puts `text` on the clipboard.
///
/// Tries the platform's tools in the order they are most likely to work, and falls back to asking the
/// terminal. An empty selection is refused rather than copied: a reader who pressed the key with
/// nothing selected should be told that, not handed a clipboard that was silently emptied.
///
/// # Errors
///
/// Returns a message when the text is empty, or when neither a tool nor the terminal could be asked
/// at all — which means the write to the interface's own output failed, and is worth reporting because
/// it is the one failure a reader can act on.
pub(crate) async fn to_clipboard(text: &str) -> Result<Copied, String> {
    if text.trim().is_empty() {
        return Err(String::from("there is nothing selected to copy"));
    }
    for (program, args) in writers() {
        if let Some(copied) = pipe_to(program, &args, text).await {
            return Ok(copied);
        }
    }
    ask_the_terminal(text)
}

/// The clipboard writers to try, most likely first.
fn writers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        return vec![("pbcopy", Vec::new())];
    }
    vec![
        ("wl-copy", Vec::new()),
        ("xclip", vec!["-selection", "clipboard"]),
    ]
}

/// Writes `text` to one tool's standard input, when that tool is there and took it.
async fn pipe_to(program: &str, args: &[&str], text: &str) -> Option<Copied> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(text.as_bytes()).await.ok()?;
        // Dropped here rather than at the end of the function, because a clipboard tool reads until
        // its input closes: waiting for it before the pipe is shut is waiting forever.
    }
    let status = child.wait().await.ok()?;
    status.success().then_some(Copied::System)
}

/// Asks the terminal to take `text`, through the interface's own output.
///
/// `OSC 52` with the `c` selection and a base64 payload, terminated by `BEL`. Written straight to
/// stdout rather than through the drawing backend: the backend writes cells, and this is a control
/// sequence that belongs between frames — which is also why it is flushed.
fn ask_the_terminal(text: &str) -> Result<Copied, String> {
    let payload = base64(text.as_bytes());
    let sequence = format!("\u{1b}]52;c;{payload}\u{7}");
    let mut out = std::io::stdout();
    out.write_all(sequence.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| error.to_string())?;
    Ok(Copied::Terminal)
}

/// The alphabet base64 is written in, in the order the encoding numbers them.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes bytes as base64, which is the only shape `OSC 52` accepts.
///
/// Hand-written for the same reason the rest of this module is: one small encoder is cheaper than a
/// dependency, and the six-bit shifts and the `=` padding are the whole of it.
#[must_use]
pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map_or(0, u32::from);
        let third = chunk.get(2).copied().map_or(0, u32::from);
        let group = first
            .wrapping_shl(16)
            .saturating_add(second.wrapping_shl(8))
            .saturating_add(third);
        let sextets = [
            (group >> 18) & 0x3f,
            (group >> 12) & 0x3f,
            (group >> 6) & 0x3f,
            group & 0x3f,
        ];
        for (index, value) in sextets.iter().enumerate() {
            // A group of one byte carries two sextets and of two bytes three; the rest are padding,
            // which is what carries the length of the payload.
            let carried = match chunk.len() {
                1 => 2,
                2 => 3,
                _ => 4,
            };
            if index >= carried {
                out.push('=');
                continue;
            }
            let symbol = usize::try_from(*value).unwrap_or(0);
            out.push(char::from(
                ALPHABET[symbol.min(ALPHABET.len().saturating_sub(1))],
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding is checked against the cases every base64 implementation is: lengths that are
    /// multiples of three, and the two that are not.
    #[test]
    fn base64_encodes_what_the_standard_says() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    /// Bytes rather than characters: a multi-byte character is three bytes and therefore four symbols,
    /// which is the case a character count would get wrong.
    #[test]
    fn the_encoder_measures_bytes_and_not_characters() {
        assert_eq!(base64("é".as_bytes()), "w6k=");
        assert_eq!(base64("日本".as_bytes()), "5pel5pys");
    }

    /// Nothing selected is refused rather than copied: a reader who pressed the key by mistake should
    /// not have their clipboard emptied.
    #[test]
    fn an_empty_selection_is_refused() {
        let (empty, blank) = nanus_kernel::runtime::block_on_local(async {
            (to_clipboard("").await, to_clipboard("   \n ").await)
        });
        assert!(empty.is_err(), "{empty:?}");
        assert!(blank.is_err(), "{blank:?}");
    }

    /// A tool that is not installed declines rather than failing the copy: this is the path that
    /// decides whether the terminal is asked, and on a machine with no `xclip` it is the ordinary one.
    #[test]
    fn a_clipboard_tool_that_is_not_there_declines() {
        let (absent, took_it, refused) = nanus_kernel::runtime::block_on_local(async {
            (
                pipe_to("definitely-not-a-clipboard-tool", &[], "x").await,
                pipe_to("true", &[], "x").await,
                pipe_to("false", &[], "x").await,
            )
        });
        assert_eq!(absent, None);
        assert_eq!(
            took_it,
            Some(Copied::System),
            "a tool that takes it says so"
        );
        // A tool that refused — a real one with nothing to select, say — is not a destination.
        assert_eq!(refused, None);
    }
}
