//! Telling a destructive tool call from a harmless one.
//!
//! The `permitted` approval state runs a call outside the sandbox without asking unless
//! the call can destroy something, so the decision "is this destructive?" has to live
//! somewhere the gate can ask, and it has to be a *decision* rather than a property of the
//! tool: `bash` is not destructive — `ls` is a `bash` call — and `rm -rf /tmp/x` is.
//!
//! ## Why deletion and not writing
//!
//! A write can replace a file's contents, and an edit can too, but they are the everyday
//! operations a coding session is made of: gating them would make the state useless for the
//! work it exists to allow. What it holds back is the call that removes something, because
//! that is the one a person cannot undo by editing again. The two filesystem tools are
//! therefore not destructive; `bash` is, when the command it runs deletes.
//!
//! ## Why temporary directories are the exception
//!
//! A scratch directory is where a destructive command is the point — a build tree is
//! removed and rebuilt — so a destructive call aimed only inside one is allowed in the
//! `permitted` state. The test is deliberately conservative: every path-looking argument a
//! command names has to be under a temporary root, and a command whose targets cannot all
//! be read that way is treated as touching something that matters.
//!
//! ## Why this is a heuristic
//!
//! The command is a string a model wrote, and a shell has more ways to delete a file than
//! a word list can hold. The classifier is therefore a *raising* of the bar rather than a
//! guarantee, and the guarantee lives where it always did: the sandbox, and a person
//! answering the prompt.

use std::path::{Component, Path, PathBuf};

use nanus_domain::ToolCall;

/// Command words that delete or overwrite data without asking.
///
/// A word matches the last path component of a token, so `/bin/rm` and `rm` both count.
/// The list is short on purpose: a word added here is one the `permitted` state stops
/// running without a person's consent, and a false negative is answered by the prompt
/// rather than by running the call.
const DESTRUCTIVE_WORDS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "truncate", "dd", "wipefs", "mkfs", "srm", "del",
];

/// Temporary roots this process will treat as scratch space.
///
/// The fixed list is the set of conventional locations, and `TMPDIR` is added when it is
/// absolute. `std::env::temp_dir` is deliberately *not* consulted here: it reads `TMPDIR`
/// on one platform and a compiled-in path on another, and the two answers to the same
/// question are harder to reason about than the question itself.
#[must_use]
pub fn temp_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(raw) = std::env::var_os("TMPDIR") {
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() {
            roots.push(candidate);
        }
    }
    roots
}

/// Returns whether a call can destroy something.
///
/// Only a `bash` call can: the filesystem tools read, write, and edit, and none of them
/// deletes. A tool added later that does delete must be named here, or the `permitted`
/// state will run it without asking.
#[must_use]
pub fn is_destructive(call: &ToolCall) -> bool {
    let Some(command) = command_of(call) else {
        return false;
    };
    // `find … -delete` deletes without naming a destructive word, and it is the one
    // non-obvious spelling worth recognising by its own flag rather than by a word.
    command_is_destructive(command) || command.contains("-delete")
}

/// Returns whether every path a destructive call names lies in a temporary directory.
///
/// `false` for a call whose targets cannot all be read that way, which includes one that
/// names no path at all: a command with nothing to check is not a command to wave through.
#[must_use]
pub fn targets_are_temporary(call: &ToolCall) -> bool {
    let Some(command) = command_of(call) else {
        return false;
    };
    let roots = temp_roots();
    let mut named = false;
    for raw in command.split_whitespace() {
        let token = raw.trim_matches(|character: char| {
            matches!(character, '\'' | '"' | ';' | '|' | '&' | '(' | ')')
        });
        let Some(path) = path_token(token) else {
            continue;
        };
        named = true;
        if !under_any(path, &roots) {
            return false;
        }
    }
    named
}

/// Returns the `command` argument of a `bash` call, if the call has one.
fn command_of(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
}

/// Returns whether any word in a command is a destructive one.
fn command_is_destructive(command: &str) -> bool {
    command.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|character: char| {
            matches!(character, '\'' | '"' | ';' | '|' | '&' | '(' | ')')
        });
        let word = token.rsplit('/').next().unwrap_or(token);
        DESTRUCTIVE_WORDS.contains(&word)
    })
}

/// Returns a path for a token that looks like one, and `None` for an option or a bare word.
///
/// Only tokens that are absolute, explicitly relative, or contain a separator are paths.
/// A bare filename is left out deliberately: `rm -rf build` names a directory beside the
/// command's working directory, and treating that as an unknown is what makes the answer
/// "not a temporary directory" rather than a guess.
fn path_token(token: &str) -> Option<&Path> {
    if token.is_empty() || token.starts_with('-') || token.contains('=') {
        return None;
    }
    let looks_like_path = token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('~');
    looks_like_path.then(|| Path::new(token))
}

/// Returns whether `path` lies inside one of `roots`.
///
/// A path containing a parent component is refused outright: `/tmp/../etc` starts with
/// `/tmp` as a sequence of components but does not name a temporary file, and a check that
/// said otherwise would be the loophole rather than the guard.
fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use nanus_domain::{ToolCall, ToolCallId, ToolName};

    use super::*;

    fn bash(command: &str) -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c-1"),
            ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name")),
            serde_json::json!({ "command": command }),
        )
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c-1"),
            ToolName::new(name).unwrap_or_else(|_| panic!("a valid tool name")),
            arguments,
        )
    }

    #[test]
    fn a_deleting_command_is_destructive_and_the_rest_are_not() {
        for command in [
            "rm -rf build",
            "rm file.txt",
            "/bin/rm -rf /tmp/x",
            "rmdir empty",
            "unlink a",
            "shred -u secret",
            "truncate -s 0 log",
            "dd if=/dev/zero of=disk",
            "find . -name '*.tmp' -delete",
            "cd /work && rm -rf node_modules",
        ] {
            assert!(is_destructive(&bash(command)), "{command} is destructive");
        }
        for command in ["ls -la", "cargo test", "grep -rn todo ."] {
            assert!(!is_destructive(&bash(command)), "{command} is harmless");
        }
    }

    #[test]
    fn the_filesystem_tools_are_not_destructive() {
        // Writing and editing are the operations a coding session is made of; gating
        // them would make the `permitted` state useless for actual work.
        assert!(!is_destructive(&call(
            "write",
            serde_json::json!({ "file_path": "src/main.rs", "content": "" })
        )));
        assert!(!is_destructive(&call(
            "edit",
            serde_json::json!({ "file_path": "src/main.rs", "old": "a", "new": "b" })
        )));
        assert!(!is_destructive(&call(
            "read",
            serde_json::json!({ "file_path": "/etc/passwd" })
        )));
    }

    #[test]
    fn only_a_command_whose_targets_are_all_temporary_is_exempt() {
        assert!(targets_are_temporary(&bash("rm -rf /tmp/build")));
        assert!(targets_are_temporary(&bash("rm -rf /var/tmp/a /tmp/b")));
        // A path that only *starts* with the letters of a temporary root is not inside it.
        assert!(!targets_are_temporary(&bash("rm -rf /tmpfile")));
        // A relative target is beside the workspace, not in a scratch directory.
        assert!(!targets_are_temporary(&bash("rm -rf ./build")));
        assert!(!targets_are_temporary(&bash("rm -rf build")));
        assert!(!targets_are_temporary(&bash("rm -rf /etc/passwd")));
        // A command that names no path cannot be shown to be temporary.
        assert!(!targets_are_temporary(&bash("rm -rf")));
        // One target outside the temporary root is enough to withdraw the exemption.
        assert!(!targets_are_temporary(&bash("rm -rf /tmp/a /etc/b")));
    }

    #[test]
    fn a_path_that_climbs_out_of_a_temporary_root_is_not_exempt() {
        assert!(!targets_are_temporary(&bash("rm -rf /tmp/../etc")));
    }

    #[test]
    fn a_parent_directory_component_is_never_under_a_root() {
        // The negative case for `under_any` on its own, so the rule is pinned even if the
        // token-level test changes.
        let roots = vec![PathBuf::from("/tmp")];
        assert!(under_any(Path::new("/tmp/a/b"), &roots));
        assert!(!under_any(Path::new("/tmp/../etc"), &roots));
        assert!(!under_any(Path::new("tmp/a"), &roots));
    }
}
