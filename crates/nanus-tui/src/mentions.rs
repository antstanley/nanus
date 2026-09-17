//! `@` mentions: finding the file a reader is naming, and completing it.
//!
//! ## What a mention is
//!
//! A word in the prompt that opens with `@` and has no whitespace in it. The caret may be
//! anywhere inside or at the end of it, so the word is read in both directions from the caret:
//! `@src/ma|in.rs` is one mention whose query is `src/main.rs`, and a reader who moves the caret
//! back into a name they had already typed gets the same list.
//!
//! ## Why the path and not the contents
//!
//! A mention expands to the path, which is text the model then has a tool to act on. Inlining the
//! file's contents would put a file the model never asked for into every request, spend the
//! context window on it, and make the prompt a thing the reader cannot see all of. The tools are
//! how a model reads a file; a mention is how it is told which file to read.
//!
//! ## What the completion offers, and what it does not
//!
//! A bounded walk of the workspace, sorted, with three names skipped — `.git`, `target`, and
//! `node_modules` — because they are machinery rather than sources. It does *not* read
//! `.gitignore`: a file a reader has deliberately ignored is still a file they may want to name.
//! A directory of a hundred thousand files is bounded rather than listed: the walk stops at
//! [`MAX_FILES`], so a completion is never slower than a keystroke.
//!
//! This module is pure: [`list`] reads a directory, but nothing here draws, and everything the
//! view needs — the query, the ranking — is a function of strings.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says which surface these items are meant for, and this is the lint's counterpart —
// the same allow `paste.rs`, `help.rs`, and `markdown/mod.rs` carry.
#![allow(clippy::redundant_pub_crate)]

use std::path::Path;

/// How many files the walk collects before it stops.
pub(crate) const MAX_FILES: usize = 2_000;

/// How deep the walk goes.
///
/// Six is past every layout a project actually has — `crates/a/src/b/mod/x.rs` is five — and the
/// bound is what keeps a generated tree from being walked for a keystroke.
const MAX_DEPTH: usize = 6;

/// Directories the walk does not descend into.
///
/// Machinery rather than sources: a build directory and a dependency cache are the two places a
/// completion would offer a thousand names nobody meant. `.git` is included because its objects
/// are not files a reader can name.
const SKIPPED: &[&str] = &[".git", "target", "node_modules"];

/// The mention the caret is inside, if it is inside one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Mention {
    /// Characters before the caret that belong to it, the `@` included.
    ///
    /// What accepting has to remove with backspaces, because the composer deletes text either side
    /// of the caret rather than a range.
    pub(crate) before: usize,
    /// Characters after the caret that belong to it.
    ///
    /// Removed forwards, for the same reason: a caret in the middle of a name still replaces the
    /// whole name.
    pub(crate) after: usize,
    /// What follows the `@`, which is what the completion is asked for.
    pub(crate) query: String,
}

/// Reads the mention at `cursor`, when the caret is in one.
///
/// `cursor` is a character offset into `text`, which is what the composer's own cursor is: a byte
/// offset would be wrong by one for every multi-byte character before the caret.
#[must_use]
pub(crate) fn at_cursor(text: &str, cursor: usize) -> Option<Mention> {
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return None;
    }
    let start = (0..cursor)
        .rev()
        .take_while(|index| !chars.get(*index).is_some_and(|c| c.is_whitespace()))
        .last()
        .unwrap_or(cursor);
    if chars.get(start) != Some(&'@') {
        return None;
    }
    let end = (cursor..chars.len())
        .take_while(|index| !chars.get(*index).is_some_and(|c| c.is_whitespace()))
        .last()
        .map_or(cursor, |index| index.saturating_add(1));
    let query: String = chars[start.saturating_add(1)..end].iter().collect();
    Some(Mention {
        before: cursor.saturating_sub(start),
        after: end.saturating_sub(cursor),
        query,
    })
}

/// Lists the files under `root`, as paths relative to it, in a stable order.
///
/// Every path uses `/` as its separator, whatever the platform writes, because these are paths for
/// a prompt: the model is told a relative path and the tools split it on `/`.
#[must_use]
pub(crate) fn list(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut queue: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        if depth > MAX_DEPTH || found.len() >= MAX_FILES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKIPPED.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            // A directory is descended into and a file is offered; anything else — a socket, a
            // device, a dangling symlink — is neither, which is what `is_dir`/`is_file` on the
            // entry's own metadata says.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                queue.push((path, depth.saturating_add(1)));
            } else if kind.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
            if found.len() >= MAX_FILES {
                break;
            }
        }
    }
    found.sort();
    found
}

/// Returns the files that match `query`, best first, at most `limit` of them.
///
/// The order is what makes a completion usable: a name that *starts* with what was typed comes
/// before one that merely contains it, and the file's own name counts for more than its
/// directory — `@main` offers `src/main.rs` before `docs/maintenance.md`. Two matches that rank
/// alike are ordered by how long the file's own name is, shortest first, which is the tighter
/// reading of what was typed; anything still tied keeps the listing's order, so the same query
/// always offers the same list.
#[must_use]
pub(crate) fn matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let mut ranked: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| rank(file, &query).map(|score| (score, name_len(file), file)))
        .collect();
    // A stable sort, so files that rank alike and are named alike come out in the listing's order
    // rather than in whatever order the walk happened to produce.
    ranked.sort_by_key(|(score, length, _)| (*score, *length));
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, file)| file.clone())
        .collect()
}

/// How long a file's own name is, which breaks a tie between two equally good matches.
///
/// `main.rs` comes before `maintenance.md` for the query `main`: both names begin with it, and the
/// shorter one is the tighter reading of what was typed.
fn name_len(file: &str) -> usize {
    file.rsplit('/').next().map_or(0, str::len)
}

/// How well one file answers a query, or `None` when it does not.
///
/// Lower is better. The ranks are the ways a reader means a name: the file's name begins with it,
/// the name contains it, the whole path contains it, and the path's *segments* begin with it.
fn rank(file: &str, query: &str) -> Option<u8> {
    let lowered = file.to_lowercase();
    let name = lowered.rsplit('/').next().unwrap_or(&lowered);
    if name.starts_with(query) {
        return Some(0);
    }
    if name.contains(query) {
        return Some(1);
    }
    if lowered.contains(query) {
        return Some(2);
    }
    if shorthand(&lowered, query) {
        return Some(3);
    }
    None
}

/// Whether every character of `query` begins a path segment of `file`, in order.
///
/// The shorthand a reader types for a path they know: `c/n/s/replay` for
/// `crates/nanus-tui/src/replay.rs`. It is last because it matches the most, and it is only tried
/// when the query has no separator of its own — a query with one is a path already.
fn shorthand(file: &str, query: &str) -> bool {
    if query.contains('/') || query.len() < 2 {
        return false;
    }
    let mut wanted = query.chars();
    let mut current = wanted.next();
    for segment in file.split('/') {
        let Some(initial) = segment.chars().next() else {
            continue;
        };
        if current == Some(initial) {
            current = wanted.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace with a handful of files, including the directories the walk skips.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for path in [
            "src/main.rs",
            "src/lib.rs",
            "docs/maintenance.md",
            "crates/tui/src/replay.rs",
            "target/debug/junk.rs",
            ".git/config",
            "node_modules/pkg/index.js",
        ] {
            let full = dir.path().join(path);
            let parent = full
                .parent()
                .map_or_else(|| dir.path().to_path_buf(), Path::to_path_buf);
            std::fs::create_dir_all(parent).expect("a directory");
            std::fs::write(&full, "").expect("a file");
        }
        dir
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn the_walk_lists_files_and_skips_the_machinery() {
        let dir = workspace();
        let files = list(dir.path());
        assert!(files.contains(&String::from("src/main.rs")), "{files:?}");
        assert!(
            files.contains(&String::from("docs/maintenance.md")),
            "{files:?}"
        );
        for skipped in [
            "target/debug/junk.rs",
            ".git/config",
            "node_modules/pkg/index.js",
        ] {
            assert!(
                !files.contains(&String::from(skipped)),
                "{skipped} is machinery, not a source: {files:?}"
            );
        }
        // Sorted, so the same workspace offers the same list twice running.
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        assert_eq!(files, list(dir.path()));
    }

    #[test]
    fn a_root_that_is_not_there_lists_nothing_rather_than_failing() {
        assert!(list(Path::new("/not/a/workspace/at/all")).is_empty());
    }

    /// The caret reads the mention in both directions: a reader who moves back into a name they
    /// typed still has the whole name as the query.
    #[test]
    fn a_mention_is_the_whole_word_the_caret_is_inside() {
        let mention = at_cursor("look at @src/main.rs please", 15).expect("a mention");
        assert_eq!(mention.query, "src/main.rs");
        assert_eq!(
            mention.before, 7,
            "the `@` and `src/ma` — what is behind the caret"
        );
        assert_eq!(mention.after, 5, "`in.rs` — what is in front of it");

        // At the very end of the word, with nothing after it.
        let at_end = at_cursor("look at @src/main.rs", 20).expect("a mention");
        assert_eq!(at_end.query, "src/main.rs");
        assert_eq!(at_end.after, 0);
        assert_eq!(at_end.before, 12);

        // A word that does not open with `@` is not a mention, which is what keeps an email
        // address a word rather than a completion.
        assert_eq!(at_cursor("write to me@example.com", 20), None);
        assert_eq!(at_cursor("a prompt with no mention", 5), None);
        // An `@` on its own is a mention with an empty query: the reader has just typed it and is
        // about to be shown everything.
        let bare = at_cursor("@", 1).expect("a mention");
        assert_eq!(bare.query, "");
        // A caret past the end of the text is not a mention anywhere.
        assert_eq!(at_cursor("@a", 9), None);
    }

    /// The ranking is what makes the list usable: the name beats the directory, and a prefix beats
    /// a substring.
    #[test]
    fn a_completion_ranks_the_name_above_the_path() {
        let files = paths(&[
            "docs/maintenance.md",
            "src/main.rs",
            "vendor/main-utils/thing.rs",
            "crates/tui/src/replay.rs",
        ]);
        let found = matches(&files, "main", 10);
        assert_eq!(
            found.first().map(String::as_str),
            Some("src/main.rs"),
            "a file called for comes before a document about it: {found:?}"
        );
        assert_eq!(
            found.len(),
            3,
            "the unrelated file is not offered: {found:?}"
        );
        assert!(
            !found.contains(&String::from("crates/tui/src/replay.rs")),
            "{found:?}"
        );
    }

    #[test]
    fn a_query_with_a_directory_in_it_matches_the_whole_path() {
        let files = paths(&["crates/tui/src/replay.rs", "src/main.rs"]);
        assert_eq!(
            matches(&files, "tui/src", 10),
            vec![String::from("crates/tui/src/replay.rs")]
        );
        // Case is ignored, because a reader typing a path should not have to match the case of
        // names they cannot see yet.
        assert_eq!(
            matches(&files, "TUI", 10),
            vec![String::from("crates/tui/src/replay.rs")]
        );
    }

    /// The shorthand a reader types for a path they know: one letter per directory, in order.
    #[test]
    fn the_shorthand_a_reader_types_finds_the_path_it_stands_for() {
        let files = paths(&["crates/tui/src/replay.rs", "src/main.rs"]);
        assert_eq!(
            matches(&files, "ctr", 10),
            vec![String::from("crates/tui/src/replay.rs")],
            "crates/tui/replay"
        );
        // A single letter is not a shorthand: it is a substring, and it matches whatever contains
        // it, which is the ranking's own job rather than this rule's.
        assert!(matches(&files, "z", 10).is_empty());
        assert!(
            !matches(&files, "c", 10).is_empty(),
            "a letter is a substring match rather than a shorthand"
        );
    }

    #[test]
    fn an_empty_query_offers_the_listing_and_the_bound_is_kept() {
        let files: Vec<String> = (0..10).map(|index| format!("file{index}.rs")).collect();
        assert_eq!(matches(&files, "", 3).len(), 3);
        assert!(matches(&files, "nothing here", 10).is_empty());
    }
}
