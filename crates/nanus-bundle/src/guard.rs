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
//! `permitted` state. The test is deliberately conservative: every argument a deleting command
//! is given has to be an absolute path that resolves under a temporary root, and a command
//! whose targets cannot all be read that way is treated as touching something that matters.
//! "Cannot be read" is most of what a shell can say: a bare name, a variable, a substitution,
//! a quoted or escaped word, a home directory, a brace, a glob anywhere but the last
//! component. Skipping such a word instead of failing on it is what once let
//! `rm -rf /tmp/x src` through on the strength of its first argument.
//!
//! A target is resolved through the filesystem before it is compared, so a link under `/tmp`
//! that leads elsewhere is not a temporary path. `TMPDIR` is believed only when it names a
//! directory at least two levels down that does not contain the home directory: a `TMPDIR`
//! of `/` would otherwise make the whole machine scratch space.
//!
//! One thing this cannot see is a command that builds its own way out — `ln -s ~ /tmp/l;
//! rm -rf /tmp/l/x` names only temporary paths when it is read, and the link does not exist
//! yet. The exemption reads a command; it does not run one.
//!
//! ## Who is classified
//!
//! A tool says which question applies to it through its [`Destructive`] declaration. `bash`
//! declares [`Destructive::ByCommand`], and only such a tool is read by the command classifier
//! below. The filesystem tools declare [`Destructive::Never`]. A tool that declares nothing —
//! a host's own, registered beside the stock seven — is [`Destructive::Always`], so `permitted`
//! asks about it until its author says it cannot destroy anything.
//!
//! ## Why this is a heuristic
//!
//! The command is a string a model wrote, and a shell has more ways to delete a file than
//! a word list can hold. The classifier is therefore a *raising* of the bar rather than a
//! guarantee, and the guarantee lives where it always did: the sandbox, and a person
//! answering the prompt.
//!
//! What it does promise is that a listed word is found however the command is punctuated.
//! The command is cut at the characters a shell cuts it at — `;`, `&`, `|`, parentheses,
//! redirections, backquotes, a newline — and quotes and backslashes are dropped, so
//! `true;rm -rf src`, `echo $(rm -rf src)` and `\rm -rf src` are the same word as `rm`. What
//! it does not promise is the rest of the shell: `git clean`, a redirection that truncates,
//! `mv` over a file, and an interpreter one-liner all delete without a listed word.

use std::path::{Component, Path, PathBuf};

use nanus_domain::{Destructive, ToolCall};

/// Command words that delete or overwrite data without asking.
///
/// A word matches the last path component of a token, so `/bin/rm` and `rm` both count.
/// The list is short on purpose: a word added here is one the `permitted` state stops
/// running without a person's consent, and a word missing from it is a command that runs
/// unasked, which is why the module calls this a heuristic.
const DESTRUCTIVE_WORDS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "truncate", "dd", "wipefs", "mkfs", "srm", "del",
];

/// The conventional scratch locations, before `TMPDIR` is considered.
const FIXED_TEMP_ROOTS: [&str; 4] = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"];

/// How many directories deep a `TMPDIR` must be before it is believed.
///
/// One level is `/tmp` — already in the fixed list — or a top-level directory that holds
/// everything else; the per-user directories platforms hand out are deeper than that.
const TMPDIR_DEPTH_MIN: usize = 2;

/// Temporary roots this process will treat as scratch space.
///
/// The fixed list is the set of conventional locations, and `TMPDIR` is added when it passes
/// [`is_scratch_root`]. `std::env::temp_dir` is deliberately *not* consulted here: it reads `TMPDIR`
/// on one platform and a compiled-in path on another, and the two answers to the same
/// question are harder to reason about than the question itself.
#[must_use]
pub fn temp_roots() -> Vec<PathBuf> {
    roots_from(
        std::env::var_os("TMPDIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

/// Builds the root list from the two environment values it depends on.
fn roots_from(tmpdir: Option<PathBuf>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = FIXED_TEMP_ROOTS.iter().map(PathBuf::from).collect();
    if let Some(candidate) = tmpdir {
        let resolved = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if is_scratch_root(&candidate, home) && is_scratch_root(&resolved, home) {
            roots.push(candidate);
        }
    }
    roots
}

/// Returns whether a `TMPDIR` value is a plausible scratch directory.
///
/// The variable is the environment's, and the exemption it widens is the one that lets a
/// deletion run unasked, so it is believed only when it could not be most of the machine:
/// absolute, with no `..`, at least [`TMPDIR_DEPTH_MIN`] directories deep, and not a
/// directory the home directory is inside.
fn is_scratch_root(candidate: &Path, home: Option<&Path>) -> bool {
    if !candidate.is_absolute() {
        return false;
    }
    let mut depth = 0_usize;
    for component in candidate.components() {
        match component {
            Component::ParentDir => return false,
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }
    depth >= TMPDIR_DEPTH_MIN && !home.is_some_and(|home| home.starts_with(candidate))
}

/// Returns whether a call to a tool that declared `declared` can destroy something.
///
/// This is the question the approval gate asks: a declaration of `never` or `always` is the
/// answer, and `by_command` defers to [`is_destructive`] on the call's own command.
#[must_use]
pub fn call_is_destructive(declared: Destructive, call: &ToolCall) -> bool {
    match declared {
        Destructive::Never => false,
        Destructive::ByCommand => is_destructive(call),
        Destructive::Always => true,
    }
}

/// Returns whether the `permitted` state runs a call to a tool that declared `declared`
/// without asking.
///
/// A tool that cannot destroy anything runs, and one that always can is asked about. A
/// command-classified call runs when its command deletes nothing, or when everything it names
/// is inside a temporary directory. The temporary exemption is for commands only: it reads
/// paths out of a command line, and a tool that is destructive by declaration has no command
/// line to read them from.
#[must_use]
pub fn permitted_runs(declared: Destructive, call: &ToolCall) -> bool {
    match declared {
        Destructive::Never => true,
        Destructive::ByCommand => !is_destructive(call) || targets_are_temporary(call),
        Destructive::Always => false,
    }
}

/// Returns whether a call's shell command can destroy something.
///
/// The classifier for a tool that declares [`Destructive::ByCommand`], which is `bash`. It
/// reads the call's `command` argument and nothing else, so a call with no command is not
/// destructive by this test: the gate asks it only of a tool that said its calls carry one.
#[must_use]
pub fn is_destructive(call: &ToolCall) -> bool {
    let Some(command) = command_of(call) else {
        return false;
    };
    // `find … -delete` deletes without naming a destructive word, and it is the one
    // non-obvious spelling worth recognising by its own flag rather than by a word.
    command_is_destructive(command) || command.contains(DELETE_FLAG)
}

/// The flag that makes `find` a deleting command.
const DELETE_FLAG: &str = "-delete";

/// Returns whether every path a destructive call names lies in a temporary directory.
///
/// `false` for a call whose targets cannot all be read that way, which includes one that
/// names no path at all: a command with nothing to check is not a command to wave through.
#[must_use]
pub fn targets_are_temporary(call: &ToolCall) -> bool {
    command_of(call).is_some_and(|command| targets_within(command, &temp_roots()))
}

/// Returns whether every argument of every deleting command in `command` is under `roots`.
///
/// Three things withdraw the exemption outright, before any path is looked at: a character
/// whose meaning depends on the shell that will run it ([`is_opaque`]), a destructive word
/// that is not the command of its own segment (`xargs rm`, `sudo rm`, `FOO=1 rm` — the
/// words after it are then someone else's arguments), and a deleting command that names
/// nothing. After that every argument that is not an option has to be a temporary path;
/// there is no kind of word that is skipped.
fn targets_within(command: &str, roots: &[PathBuf]) -> bool {
    if command.chars().any(is_opaque) {
        return false;
    }
    let mut named = false;
    for words in segments(command) {
        let Some((name, arguments)) = words.split_first() else {
            continue;
        };
        if arguments.iter().any(|word| names_destructive_word(word)) {
            return false;
        }
        let deletes =
            names_destructive_word(name) || arguments.iter().any(|word| word.contains(DELETE_FLAG));
        if !deletes {
            continue;
        }
        // After `--` a word that starts with a dash is a file name, not an option.
        let mut options_ended = false;
        for argument in arguments {
            if argument == "--" {
                options_ended = true;
                continue;
            }
            if !options_ended && argument.starts_with('-') {
                continue;
            }
            // `of=/dev/disk0` names a target as surely as a bare path does.
            let target = argument
                .split_once('=')
                .map_or(argument.as_str(), |(_, value)| value);
            if !is_temporary_target(target, roots) {
                return false;
            }
            named = true;
        }
    }
    named
}

/// Returns whether `target` is an absolute path that resolves strictly inside one of `roots`.
///
/// A glob is read only as the last component, where what it expands to is by construction a
/// child of a directory that was itself checked; anywhere else, or before a trailing slash,
/// it can match a link and be followed through it.
fn is_temporary_target(target: &str, roots: &[PathBuf]) -> bool {
    let has_glob = |text: &str| text.contains(['*', '?']);
    if !has_glob(target) {
        return under_any(Path::new(target), roots);
    }
    let Some((parent, last)) = target.rsplit_once('/') else {
        return false;
    };
    if has_glob(parent) || last.is_empty() {
        return false;
    }
    // `/tmp/*` empties the root rather than removing it, so the parent may be the root.
    resolves_under(Path::new(parent), roots, true)
}

/// Returns whether `path` lies strictly inside one of `roots`.
///
/// A path containing a parent component is refused outright: `/tmp/../etc` starts with
/// `/tmp` as a sequence of components but does not name a temporary file, and a check that
/// said otherwise would be the loophole rather than the guard.
fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    resolves_under(path, roots, false)
}

/// Returns whether `path`, resolved through the filesystem, lies inside one of `roots`.
///
/// Both sides are resolved, because `/tmp` is itself a link on some platforms and a target
/// written under it would otherwise never compare equal to where it really is. The root
/// itself is not inside the root unless `or_root` says the caller is naming its contents.
fn resolves_under(path: &Path, roots: &[PathBuf], or_root: bool) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    let resolved = resolve_existing(path);
    let inside = |root: &Path| resolved.starts_with(root) && (or_root || resolved != root);
    roots.iter().any(|root| {
        inside(root)
            || root
                .canonicalize()
                .is_ok_and(|resolved_root| inside(&resolved_root))
    })
}

/// Resolves the part of `path` that exists, and keeps the rest as written.
///
/// A target of a deletion often does not exist — that is a no-op, not a danger — but the
/// directories leading to it usually do, and one of them may be a link out of the
/// temporary root. The longest existing ancestor is therefore resolved and the remainder
/// appended, which is the same shape the filesystem adapter uses for a file not yet written.
fn resolve_existing(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(mut resolved) = existing.canonicalize() {
            resolved.extend(tail.iter().rev());
            return resolved;
        }
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        existing = parent;
    }
}

/// Returns the `command` argument of a `bash` call, if the call has one.
fn command_of(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
}

/// Returns whether any word in a command is a destructive one.
fn command_is_destructive(command: &str) -> bool {
    segments(command)
        .iter()
        .flatten()
        .any(|word| names_destructive_word(word))
}

/// Returns whether a word is, or carries, a destructive command name.
///
/// The word is cut again at the characters that join a name to something else inside one
/// token — `$`, braces and `=` — so `rm${IFS}-rf` and `X=rm` are found, and the name is the
/// last path component of each piece, so `/bin/rm` is too.
fn names_destructive_word(word: &str) -> bool {
    word.split(['$', '{', '}', '=']).any(|piece| {
        let name = piece.rsplit('/').next().unwrap_or(piece);
        DESTRUCTIVE_WORDS.contains(&name)
    })
}

/// Cuts a command into simple commands, and each of those into words.
///
/// This is not a shell parser, and it errs in the direction that finds more: quotes and
/// backslashes are dropped rather than honoured, so a quoted separator still separates and
/// `sh -c 'rm -rf x'` still contains the word `rm`. The exemption cannot afford that
/// reading — a quoted `&` is not a separator to the shell that runs it — which is why
/// [`targets_within`] refuses a command with any quoting in it before it gets here.
fn segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    for character in command.chars() {
        if is_quoting(character) {
            continue;
        }
        let separates = is_separator(character);
        if !separates && !character.is_whitespace() {
            word.push(character);
            continue;
        }
        if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if separates && !words.is_empty() {
            segments.push(std::mem::take(&mut words));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if !words.is_empty() {
        segments.push(words);
    }
    segments
}

/// Whether a character ends one simple command and starts another.
const fn is_separator(character: char) -> bool {
    matches!(
        character,
        ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`' | '\n'
    )
}

/// Whether a character is one the shell removes while it reads a word.
const fn is_quoting(character: char) -> bool {
    matches!(character, '\'' | '"' | '\\')
}

/// Whether a character makes a word mean something only the running shell can say.
///
/// Expansion (`$`, a backquote, `~`, braces, a bracket glob) turns a word into a different
/// one, and quoting changes where a word ends. A command containing any of them is not one
/// whose targets can be read off the page.
const fn is_opaque(character: char) -> bool {
    is_quoting(character) || matches!(character, '$' | '`' | '~' | '{' | '}' | '[' | ']')
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
    fn a_declaration_decides_and_only_a_command_tool_is_read() {
        let deleting = bash("rm -rf /work/src");
        let harmless = bash("ls");
        let scratch = bash("rm -rf /tmp/build");
        // `never` and `always` are the answer whatever the arguments say.
        for call in [&deleting, &harmless, &scratch] {
            assert!(!call_is_destructive(Destructive::Never, call));
            assert!(permitted_runs(Destructive::Never, call));
            assert!(call_is_destructive(Destructive::Always, call));
            assert!(!permitted_runs(Destructive::Always, call));
        }
        // `by_command` reads the command, and keeps the temporary-directory exemption.
        assert!(call_is_destructive(Destructive::ByCommand, &deleting));
        assert!(!permitted_runs(Destructive::ByCommand, &deleting));
        assert!(!call_is_destructive(Destructive::ByCommand, &harmless));
        assert!(permitted_runs(Destructive::ByCommand, &harmless));
        assert!(permitted_runs(Destructive::ByCommand, &scratch));
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

    /// The defect this pins: a word that did not look like a path was skipped, so one
    /// temporary argument carried a whole command past the gate.
    #[test]
    fn a_target_that_cannot_be_read_as_a_temporary_path_withdraws_the_exemption() {
        for command in [
            "rm -rf /tmp/x src",
            "rm -rf /tmp/x $HOME",
            "rm -rf /tmp/x *",
            "find /tmp/x . -delete",
            "cd /tmp/x && rm -rf \"$OLDPWD\"",
            "rm -rf /tmp/${X:-..}/Users/me/work",
            "rm -rf /tmp/x ~/work",
            "rm -rf /tmp/x '&' /etc",
            "rm -rf /tmp/x\\ /etc",
            "rm -rf /tmp/x {src,docs}",
            "dd if=/tmp/image of=/dev/disk0",
            "rm -rf /tmp/x -- -/../..",
            // A destructive word that is not its segment's command has arguments nobody read.
            "ls /tmp/x | xargs rm",
            "FOO=1 rm -rf /tmp/x",
            // A glob is read only as the last component.
            "rm -rf /tmp/*/cache",
            "rm -rf /tmp/build/*/",
            // The root itself is not inside the root.
            "rm -rf /tmp",
            "rm -rf /tmp/",
        ] {
            assert!(
                !targets_are_temporary(&bash(command)),
                "{command} is not shown to be temporary"
            );
        }
        // The neighbours that must still be waved through.
        for command in [
            "rm -rf /tmp/x",
            "rm -rf /tmp/build/",
            "rm -rf /tmp/build/*",
            "rm -rf /tmp/*",
            "rm -rf -- /tmp/x",
            "/bin/rm -rf /tmp/x",
            "cd /work && rm -rf /tmp/x; ls src",
            "find /tmp/x -delete",
            "dd if=/tmp/a of=/tmp/b",
            "rm -rf /tmp/x > log 2>&1",
        ] {
            assert!(
                targets_are_temporary(&bash(command)),
                "{command} names only temporary paths"
            );
        }
    }

    #[test]
    fn a_destructive_word_is_found_however_the_command_is_punctuated() {
        for command in [
            "true;rm -rf src",
            "cd /work&&rm -rf node_modules",
            "ls|xargs rm",
            "echo $(rm -rf src)",
            "echo `rm -rf src`",
            "(rm -rf src)",
            "{ rm -rf src; }",
            "\\rm -rf src",
            "'rm' -rf src",
            "r\"\"m -rf src",
            "rm${IFS}-rf${IFS}src",
            "X=rm; $X -rf src",
            "sh -c 'rm -rf src'",
            "echo hi\nrm -rf src",
            "echo hi >log;rm -rf src",
        ] {
            assert!(is_destructive(&bash(command)), "{command} is destructive");
        }
        // A listed word inside a longer one is not the word.
        for command in [
            "docker run --rm image",
            "cargo run --bin trim",
            "ls dd.txt format",
            "git commit -m 'add'",
        ] {
            assert!(!is_destructive(&bash(command)), "{command} is harmless");
        }
    }

    /// A link under a temporary root that leads elsewhere is resolved before it is compared.
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_a_temporary_root_is_not_a_temporary_path() {
        let scratch = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let root = scratch
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical: {error}"));
        std::fs::create_dir(root.join("real")).unwrap_or_else(|error| panic!("mkdir: {error}"));
        // `/usr` exists on every Unix this runs on and is never scratch space.
        std::os::unix::fs::symlink("/usr", root.join("link"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));
        let roots = vec![root.clone()];
        let through = format!("rm -rf {}/link/share", root.display());
        assert!(!targets_within(&through, &roots), "{through}");
        let trailing = format!("rm -rf {}/link/", root.display());
        assert!(!targets_within(&trailing, &roots), "{trailing}");
        // The directory beside it, and a path that does not exist yet, are still inside.
        let inside = format!("rm -rf {}/real/cache", root.display());
        assert!(targets_within(&inside, &roots), "{inside}");
        let absent = format!("rm -rf {}/absent/cache", root.display());
        assert!(targets_within(&absent, &roots), "{absent}");
    }

    #[test]
    fn a_tmpdir_that_could_be_most_of_the_machine_is_not_a_temporary_root() {
        let home = Path::new("/home/someone");
        for refused in [
            "/",
            "/home",
            "/home/someone",
            "relative/tmp",
            "/a/../b/c",
            "/scratch",
        ] {
            let roots = roots_from(Some(PathBuf::from(refused)), Some(home));
            assert!(
                !roots.contains(&PathBuf::from(refused)),
                "{refused} is not believed"
            );
            assert!(!targets_within("rm -rf /home/someone/work", &roots));
        }
        // The per-user directory a platform hands out is believed, with or without a home.
        let given = PathBuf::from("/nonexistent-volume/folders/ab/T");
        for home in [Some(home), None] {
            let roots = roots_from(Some(given.clone()), home);
            assert!(roots.contains(&given));
            assert!(targets_within(
                "rm -rf /nonexistent-volume/folders/ab/T/build",
                &roots
            ));
        }
        assert!(roots_from(None, Some(home)).contains(&PathBuf::from("/tmp")));
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
