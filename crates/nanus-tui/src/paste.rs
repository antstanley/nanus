//! Reading an image off the clipboard and putting it where the tools can reach it.
//!
//! ## Why a file, and why inside the workspace
//!
//! `read_image` takes a *path*: the wire has a way to carry an image to the model — a tool result
//! with a content block — but nothing that carries pasted bytes from a client to an agent, and
//! inventing one would mean the link could carry arbitrary bytes in a frame the interface
//! produces. So a pasted image is written into the workspace and its path is put in the prompt,
//! exactly as if the reader had typed that path. The consequence is worth knowing: the file lands
//! in the reader's own directory, under `.nanus/pasted/`, and it stays there. A directory rather
//! than a temp file because the tools are rooted at the workspace root and a path outside it is
//! refused — an image pasted into `/tmp` would be one the model could not read.
//!
//! ## Why the platform's own tools
//!
//! A terminal has no clipboard API: reading one is a platform call, and the crates that make it
//! (`arboard` and friends) pull in windowing systems this program never opens. The command-line
//! readers are already on the machine — `pbpaste` ships with macOS, `wl-paste` and `xclip` are
//! what a Wayland or X session provides — so the interface asks one of those, and the answer is
//! checked by its magic number rather than trusted: a reader that returned the clipboard's *text*
//! is not an image, and is treated as no image at all.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says which surface these items are meant for, and this is the lint's counterpart.
// `markdown/mod.rs` and `help.rs` carry the same allow for the same reason.
#![allow(clippy::redundant_pub_crate)]

use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The directory pasted images are written under, relative to the workspace root.
///
/// Named after the harness rather than after the image, because it is the harness's directory:
/// a reader who finds it should be able to tell what put it there.
pub(crate) const PASTED_DIR: &str = ".nanus/pasted";

/// The counter that keeps two pastes in one process apart.
///
/// A counter rather than a clock: two images pasted in the same millisecond are the ordinary
/// case for a reader pasting a screenshot and then a crop of it, and a clock with a coarser
/// resolution than that would have the second overwrite the first.
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Returns the image on the clipboard, if there is one.
///
/// Tries the platform's readers in the order they are most likely to work and returns the first
/// answer that is an image. `None` covers every way there can be no image — no reader installed,
/// nothing on the clipboard, or something that is not an image — because the reader is told the
/// same thing in all three cases: there is nothing to paste.
#[must_use]
pub(crate) fn from_clipboard() -> Option<Vec<u8>> {
    readers().into_iter().find_map(|(program, args)| {
        run(program, &args).filter(|bytes| extension_of(bytes).is_some())
    })
}

/// The clipboard readers to try, most likely first.
fn readers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        return vec![
            ("pbpaste", vec!["-Prefer", "png"]),
            // The one a reader who has it installed may prefer: it fails rather than answering
            // with text when the clipboard holds no image.
            ("pngpaste", vec!["-"]),
        ];
    }
    vec![
        ("wl-paste", vec!["--type", "image/png"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ]
}

/// Runs one reader, returning what it printed when it succeeded.
fn run(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(output.stdout)
}

/// Returns the file extension for an image, from its magic number.
///
/// The bytes decide, not the tool that produced them: a clipboard reader that answered with the
/// clipboard's text must not have that text written to a file called `.png`, where the model
/// would be shown it as an image.
#[must_use]
pub(crate) fn extension_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    // `RIFF....WEBP`: the four bytes after the magic are the file's length and are not checked,
    // because the length is not what decides whether the model can read it.
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("webp");
    }
    None
}

/// Writes an image into `root`, returning the path to paste into the prompt.
///
/// The path is *relative* to the root, because that is what the tools take: a rooted filesystem
/// refuses an absolute path outside it, and the relative form is what a reader would have typed.
///
/// # Errors
///
/// Returns the underlying error when the directory cannot be created or the file cannot be
/// written, and `InvalidInput` when the bytes are not an image the model could read.
pub(crate) fn store(bytes: &[u8], root: &Path) -> std::io::Result<String> {
    let Some(extension) = extension_of(bytes) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the clipboard did not hold an image",
        ));
    };
    let dir = root.join(PASTED_DIR);
    std::fs::create_dir_all(&dir)?;
    // A name that is already taken is skipped rather than overwritten: `write` truncates, and the
    // file it would truncate is somebody's earlier paste.
    for _ in 0..MAX_ATTEMPTS {
        let name = next_name(extension);
        let path = dir.join(&name);
        if path.exists() {
            continue;
        }
        // `create_new`, so two pastes racing for the same name cannot both win: the loser gets
        // `AlreadyExists` and asks for another name.
        match std::fs::File::create_new(&path) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.flush()?;
                return Ok(format!("{PASTED_DIR}/{name}"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free name for a pasted image",
    ))
}

/// How many names to try before giving up.
///
/// The counter is unique within a process and the process id separates processes, so reaching this
/// bound means something else is filling the directory with exactly these names.
const MAX_ATTEMPTS: u32 = 64;

/// Returns a name no earlier paste in this process has asked for.
fn next_name(extension: &str) -> String {
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("image-{}-{count}.{extension}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"the rest of a png");
        bytes
    }

    #[test]
    fn an_image_is_recognised_by_its_magic_number() {
        assert_eq!(extension_of(&png()), Some("png"));
        assert_eq!(extension_of(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(extension_of(b"GIF89a...."), Some("gif"));
        assert_eq!(extension_of(b"RIFF\0\0\0\0WEBP"), Some("webp"));
    }

    /// The negative cases are the ones that matter: a reader that answered with text, or nothing,
    /// must not become a file the model is told is an image.
    #[test]
    fn something_that_is_not_an_image_is_not_an_image() {
        assert_eq!(extension_of(b""), None);
        assert_eq!(extension_of(b"just some text"), None);
        assert_eq!(
            extension_of(b"RIFF\0\0\0\0WAVE"),
            None,
            "a sound, not a picture"
        );
        assert_eq!(extension_of(b"\x89PNG"), None, "a truncated header");
    }

    #[test]
    fn a_stored_image_lands_inside_the_workspace_and_is_where_it_says_it_is() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let relative = store(&png(), root).expect("the image is stored");
        assert!(relative.starts_with(PASTED_DIR), "{relative}");
        assert!(!relative.starts_with('/'), "a path the tools can read");
        let written = root.join(&relative);
        assert_eq!(std::fs::read(&written).expect("the file is there"), png());

        // Two pastes are two files, which is the whole reason the name carries a counter.
        let second = store(&png(), root).expect("the second image is stored");
        assert_ne!(relative, second);
        assert!(root.join(&second).exists());
    }

    #[test]
    fn something_that_is_not_an_image_is_refused_rather_than_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let refused = store(b"a paragraph of text", dir.path());
        assert!(refused.is_err(), "{refused:?}");
        assert!(
            !dir.path().join(PASTED_DIR).exists(),
            "nothing was created for it"
        );
    }

    /// A workspace that cannot hold the file is an error the caller reports, not a panic.
    #[test]
    fn a_workspace_that_cannot_be_written_to_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("not a directory");
        std::fs::write(&file, b"x").expect("the file is written");
        assert!(store(&png(), &file).is_err());
    }

    /// A reader that is not installed, or that answers with something that is not an image, is
    /// no image — the caller says "nothing to paste" either way, which is true of both.
    #[test]
    fn a_clipboard_reader_that_answers_with_text_has_no_image() {
        assert_eq!(run("definitely-not-a-clipboard-tool", &[]), None);
        let echo = run("echo", &["not an image"]);
        assert!(
            echo.as_ref()
                .is_none_or(|bytes| extension_of(bytes).is_none()),
            "text is not an image: {echo:?}"
        );
        // And the whole path agrees: `from_clipboard` returning text would be the bug this
        // guards, so the check it applies is the one the reader is offered.
        assert_eq!(
            echo.as_deref().and_then(extension_of),
            None,
            "a reader that answers with text contributes nothing"
        );
    }
}
