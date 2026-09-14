//! The filesystem port, and the two rules an adapter must not soften.
//!
//! ### The exactly-once edit rule
//!
//! [`FsPort::edit`] replaces one occurrence, or all of them when `replace_all`
//! is set. A pattern that matches *nothing* is [`FsError::EditNoMatch`], and a
//! pattern that matches more than once without `replace_all` is
//! [`FsError::EditMultipleMatches`]. Neither is a silent no-op and neither is a
//! partial rewrite: a model that edits the wrong file with a too-short anchor is
//! a real failure mode, and reporting it is how the model learns.
//!
//! [`occurrence_count`] is the rule, factored out, so every adapter counts the
//! same way and a test can pin the behaviour.
//!
//! ### The workspace boundary
//!
//! [`ensure_within`] normalises a path lexically and refuses anything outside
//! the workspace root with [`FsError::OutsideWorkspace`]. It is lexical on
//! purpose: it must reject a path that does not exist yet, which a
//! canonicalising check cannot do. A harness must not let a tool write anywhere,
//! and a port that only *documents* that rule is a port that will eventually be
//! implemented without it.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::LocalBoxFuture;

/// A shared, key-addressable filesystem.
pub type FsHandle = std::rc::Rc<Box<dyn FsPort>>;

/// The filesystem port.
///
/// Every future borrows only its inputs; an implementation that needs to own its
/// state clones it into the future before returning.
pub trait FsPort {
    /// Reads a text file.
    fn read<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<FileRead>>;

    /// Writes a text file, creating or overwriting according to `mode`.
    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
        mode: WriteMode,
    ) -> LocalBoxFuture<'a, FsResult<WriteOutcome>>;

    /// Replaces text in a file, exactly once unless `replace_all`.
    fn edit<'a>(
        &'a self,
        path: &'a Path,
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    ) -> LocalBoxFuture<'a, FsResult<EditOutcome>>;

    /// Returns `true` when the path exists.
    ///
    /// Infallible by design: "does it exist" has an answer even when the answer
    /// is "the parent is unreadable", and that answer is `false`.
    fn exists<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, bool>;

    /// Returns metadata for a path.
    fn metadata<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<FileMeta>>;

    /// Lists a directory, one level deep.
    fn list<'a>(&'a self, dir: &'a Path) -> LocalBoxFuture<'a, FsResult<Vec<DirEntry>>>;

    /// Searches under a root, by glob or by literal substring.
    fn search<'a>(&'a self, query: &'a SearchQuery) -> LocalBoxFuture<'a, FsResult<SearchOutcome>>;

    /// Resolves a path to its canonical, absolute form.
    fn canonicalize<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<PathBuf>>;

    /// Reads a file's bytes.
    ///
    /// The binary counterpart of [`FsPort::read`], for a caller that needs the bytes rather
    /// than the text: an image is not UTF-8 and has no lines, so there is no `FileRead` to
    /// put it in. Confinement is the adapter's here exactly as it is there — a path outside
    /// the workspace root is refused by the port rather than by the caller remembering to
    /// ask, which is what keeps a tool from being the one place that forgets.
    fn read_bytes<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<Vec<u8>>>;
}

/// The result type of every filesystem operation.
pub type FsResult<T> = Result<T, FsError>;

/// A text file that was read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRead {
    /// The path that was read.
    pub path: PathBuf,
    /// The file's text.
    pub text: String,
    /// How many lines the text contains.
    pub total_lines: usize,
}

/// How a write treats an existing file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Refuse to replace an existing file.
    Create,
    /// Replace whatever is there.
    Overwrite,
}

/// What a write did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutcome {
    /// The path that was written.
    pub path: PathBuf,
    /// How many bytes were written.
    pub bytes_written: u64,
    /// Whether the file was created rather than replaced.
    pub created: bool,
}

/// What an edit did, with enough context to preview it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditOutcome {
    /// The path that was edited.
    pub path: PathBuf,
    /// How many occurrences were replaced.
    pub replacements: u32,
    /// The file's text before the edit.
    pub before: String,
    /// The file's text after the edit.
    pub after: String,
}

impl EditOutcome {
    /// Renders a unified diff of the edit.
    ///
    /// One hunk covering the changed region, with up to `context` unchanged
    /// lines on either side. Computing the common prefix and suffix rather than
    /// running a full diff is deliberate: an edit has exactly one changed
    /// region per occurrence by construction, so a line-diff algorithm would be
    /// more machinery for the same answer.
    #[must_use]
    pub fn unified_diff(&self) -> String {
        let before: Vec<&str> = self.before.lines().collect();
        let after: Vec<&str> = self.after.lines().collect();
        let prefix = common_prefix(&before, &after);
        let suffix = common_suffix(&before, &after, prefix);
        let removed = before.len().saturating_sub(prefix).saturating_sub(suffix);
        let added = after.len().saturating_sub(prefix).saturating_sub(suffix);
        if removed == 0 && added == 0 {
            return String::new();
        }
        let mut out = format!(
            "--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@\n",
            self.path.display(),
            self.path.display(),
            prefix.saturating_add(1),
            removed,
            prefix.saturating_add(1),
            added
        );
        for line in before.iter().skip(prefix).take(removed) {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
        for line in after.iter().skip(prefix).take(added) {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Returns `true` when the edit changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before == self.after
    }
}

/// Returns how many leading lines the two slices share.
fn common_prefix(before: &[&str], after: &[&str]) -> usize {
    let mut shared = 0_usize;
    while shared < before.len() && shared < after.len() {
        if before.get(shared) != after.get(shared) {
            break;
        }
        shared = shared.saturating_add(1);
    }
    shared
}

/// Returns how many trailing lines the two slices share, past `prefix`.
fn common_suffix(before: &[&str], after: &[&str], prefix: usize) -> usize {
    let mut shared = 0_usize;
    let limit = before
        .len()
        .saturating_sub(prefix)
        .min(after.len().saturating_sub(prefix));
    while shared < limit {
        let left = before.len().saturating_sub(shared).saturating_sub(1);
        let right = after.len().saturating_sub(shared).saturating_sub(1);
        if before.get(left) != after.get(right) {
            break;
        }
        shared = shared.saturating_add(1);
    }
    shared
}

/// A path's metadata, flattened into the three facts tools act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// The path described.
    pub path: PathBuf,
    /// Whether it is a regular file.
    pub is_file: bool,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Its length in bytes.
    pub byte_len: u64,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// The entry's full path.
    pub path: PathBuf,
    /// The entry's file name.
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Whether it is a symlink.
    pub is_symlink: bool,
    /// Its length in bytes.
    pub byte_len: u64,
}

/// How a search matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    /// Match file names against a glob.
    Glob,
    /// Match file contents against a literal substring.
    Literal,
}

/// What to search for, and where to stop.
///
/// The caps are not optional. A search over a monorepo without a result cap and
/// a per-file size cap is a way to make the harness unresponsive, so the type
/// makes both explicit and [`SearchQuery::glob`] and [`SearchQuery::literal`]
/// supply defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Where to start.
    pub root: PathBuf,
    /// The glob or the literal text.
    pub pattern: String,
    /// How to interpret `pattern`.
    pub kind: SearchKind,
    /// Maximum matches to return.
    pub max_results: usize,
    /// Skip files larger than this many bytes.
    pub max_file_bytes: u64,
    /// Whether matching is case sensitive.
    pub case_sensitive: bool,
    /// Whether hidden files are searched.
    pub include_hidden: bool,
}

impl SearchQuery {
    /// The default match ceiling.
    pub const DEFAULT_MAX_RESULTS: usize = 200;

    /// The default per-file size ceiling, in bytes.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;

    /// Builds a glob query with the default caps.
    #[must_use]
    pub fn glob(root: impl Into<PathBuf>, pattern: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            pattern: pattern.into(),
            kind: SearchKind::Glob,
            max_results: Self::DEFAULT_MAX_RESULTS,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            case_sensitive: true,
            include_hidden: false,
        }
    }

    /// Builds a literal-substring query with the default caps.
    #[must_use]
    pub fn literal(root: impl Into<PathBuf>, pattern: impl Into<String>) -> Self {
        Self {
            kind: SearchKind::Literal,
            ..Self::glob(root, pattern)
        }
    }

    /// Replaces the match ceiling.
    #[must_use]
    pub const fn with_max_results(mut self, max_results: usize) -> Self {
        self.max_results = max_results;
        self
    }

    /// Replaces the per-file size ceiling.
    #[must_use]
    pub const fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    /// Enables case-insensitive matching.
    #[must_use]
    pub const fn with_case_insensitive(mut self) -> Self {
        self.case_sensitive = false;
        self
    }

    /// Includes hidden files and directories.
    #[must_use]
    pub const fn with_hidden(mut self) -> Self {
        self.include_hidden = true;
        self
    }
}

/// One line that matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// The file the match is in.
    pub path: PathBuf,
    /// The 1-based line number.
    pub line_number: u64,
    /// The matching line's text.
    pub line: String,
}

/// What a search found, and whether it stopped early.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOutcome {
    /// The matches, in traversal order.
    pub matches: Vec<SearchMatch>,
    /// Whether the match ceiling stopped the search.
    pub truncated: bool,
    /// How many files were considered.
    pub files_scanned: u64,
}

/// How many times `needle` occurs in `haystack`.
///
/// The exactly-once edit rule depends on this count being computed the same way
/// everywhere. An empty needle counts as zero occurrences, because "replace the
/// empty string" is not a meaningful edit and would otherwise match at every
/// position.
#[must_use]
pub fn occurrence_count(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

/// Applies the exactly-once rule to a match count.
///
/// # Errors
///
/// Returns [`FsError::EditNoMatch`] for zero occurrences and
/// [`FsError::EditMultipleMatches`] for more than one when `replace_all` is not
/// set. This is the rule as a function, so an adapter that calls it cannot
/// accidentally implement a silent no-op.
pub fn check_edit_count(path: &Path, count: usize, replace_all: bool) -> FsResult<u32> {
    if count == 0 {
        return Err(FsError::EditNoMatch {
            path: path.to_path_buf(),
        });
    }
    if count > 1 && !replace_all {
        return Err(FsError::EditMultipleMatches {
            path: path.to_path_buf(),
            count: u32::try_from(count).unwrap_or(u32::MAX),
        });
    }
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Normalises a path lexically and checks it stays inside `root`.
///
/// Normalisation is lexical: `.` is dropped and `..` pops the previous
/// component, with no filesystem access at all. That is what makes the check
/// usable on a path that does not exist yet, which is exactly the case a write
/// tool presents.
///
/// # Errors
///
/// Returns [`FsError::OutsideWorkspace`] when the normalised result is not under
/// the normalised root. The comparison is component-wise, so `/work2` is not
/// mistaken for a child of `/work`.
///
/// # Panics
///
/// Panics when `root` is not absolute. A relative workspace root makes
/// confinement depend on the process's working directory, which is not a
/// property this function can check and not one a harness should rely on.
pub fn ensure_within(root: &Path, path: &Path) -> FsResult<PathBuf> {
    assert!(
        root.is_absolute(),
        "a workspace root is absolute so confinement does not depend on the cwd"
    );
    let root = normalize(root);
    let candidate = if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&root.join(path))
    };
    if candidate.starts_with(&root) {
        return Ok(candidate);
    }
    Err(FsError::OutsideWorkspace {
        root,
        path: candidate,
    })
}

/// Resolves `.` and `..` without touching the filesystem.
///
/// A `..` that would escape the root is kept as a literal component, so the
/// result stays honest about where it points and [`ensure_within`] can reject it
/// rather than silently clamping it to the root.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    // Postcondition: the result has no `.` components left to resolve.
    assert!(!out.components().any(|c| c == Component::CurDir));
    out
}

/// Why a filesystem operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FsError {
    /// The path does not exist.
    #[error("{path} does not exist")]
    NotFound {
        /// The missing path.
        path: PathBuf,
    },

    /// The path escapes the workspace root.
    #[error("{path} is outside the workspace root {root}")]
    OutsideWorkspace {
        /// The workspace root.
        root: PathBuf,
        /// The rejected path.
        path: PathBuf,
    },

    /// The path exists but is not a file.
    #[error("{path} is not a file")]
    NotAFile {
        /// The offending path.
        path: PathBuf,
    },

    /// The path is a directory where a file was wanted.
    #[error("{path} is a directory")]
    IsADirectory {
        /// The offending path.
        path: PathBuf,
    },

    /// The write would have replaced an existing file.
    #[error("{path} already exists")]
    AlreadyExists {
        /// The existing path.
        path: PathBuf,
    },

    /// The edit's pattern matched nothing.
    #[error("{path} does not contain the text to replace")]
    EditNoMatch {
        /// The file that was to be edited.
        path: PathBuf,
    },

    /// The edit's pattern matched more than once without `replace_all`.
    #[error("{path} contains {count} matches; set replace_all to replace them all")]
    EditMultipleMatches {
        /// The file that was to be edited.
        path: PathBuf,
        /// How many matches were found.
        count: u32,
    },

    /// A search pattern could not be compiled.
    #[error("invalid search pattern {pattern:?}: {reason}")]
    InvalidPattern {
        /// The rejected pattern.
        pattern: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The operating system refused access.
    #[error("permission denied for {path}: {message}")]
    Permission {
        /// The path that was refused.
        path: PathBuf,
        /// The rendered failure.
        message: String,
    },

    /// Any other input/output failure.
    #[error("I/O failure for {path}: {message}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The rendered failure.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_that_matches_nothing_is_an_error() {
        let path = Path::new("/work/a.rs");
        let outcome = check_edit_count(path, 0, false);
        assert!(matches!(outcome, Err(FsError::EditNoMatch { .. })));
        // Negative space: the same call with a match succeeds, so the failure is
        // the count and not the fixture.
        assert_eq!(check_edit_count(path, 1, false).ok(), Some(1));
    }

    #[test]
    fn an_edit_that_matches_twice_is_an_error_unless_replace_all() {
        let path = Path::new("/work/a.rs");
        let ambiguous = check_edit_count(path, 2, false);
        assert!(matches!(
            ambiguous,
            Err(FsError::EditMultipleMatches { count: 2, .. })
        ));
        assert_eq!(check_edit_count(path, 2, true).ok(), Some(2));
        assert_eq!(check_edit_count(path, 5, true).ok(), Some(5));
    }

    #[test]
    fn occurrences_are_counted_without_overlap() {
        assert_eq!(occurrence_count("aaaa", "aa"), 2);
        assert_eq!(occurrence_count("fn main()", "fn"), 1);
        assert_eq!(occurrence_count("fn main()", "nope"), 0);
        // An empty needle is not an edit target, however many positions match.
        assert_eq!(occurrence_count("abc", ""), 0);
    }

    #[test]
    fn a_path_inside_the_workspace_is_accepted() {
        let root = Path::new("/work");
        let inside = ensure_within(root, Path::new("src/lib.rs"));
        assert_eq!(inside.ok(), Some(PathBuf::from("/work/src/lib.rs")));
        let dotted = ensure_within(root, Path::new("./src/../src/lib.rs"));
        assert_eq!(dotted.ok(), Some(PathBuf::from("/work/src/lib.rs")));
        let absolute = ensure_within(root, Path::new("/work/src/lib.rs"));
        assert_eq!(absolute.ok(), Some(PathBuf::from("/work/src/lib.rs")));
        // The root itself is inside the root.
        assert_eq!(
            ensure_within(root, Path::new(".")).ok(),
            Some(PathBuf::from("/work"))
        );
    }

    #[test]
    fn a_path_that_escapes_the_workspace_is_rejected() {
        let root = Path::new("/work");
        for escape in [
            "../etc/passwd",
            "src/../../etc/passwd",
            "/etc/passwd",
            "/work/../etc/passwd",
            "..",
            "/work2/file",
        ] {
            let outcome = ensure_within(root, Path::new(escape));
            assert!(
                matches!(outcome, Err(FsError::OutsideWorkspace { .. })),
                "{escape} must be refused, got {outcome:?}"
            );
        }
    }

    #[test]
    fn a_sibling_directory_with_a_shared_prefix_is_not_inside() {
        // A string-prefix check would accept this; a component-wise check does
        // not, which is the whole reason the comparison is not `starts_with` on
        // the rendered path.
        let outcome = ensure_within(Path::new("/work"), Path::new("/workshop/secret"));
        assert!(matches!(outcome, Err(FsError::OutsideWorkspace { .. })));
    }

    #[test]
    fn normalisation_resolves_dots_without_the_filesystem() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("a/b/../../c")), PathBuf::from("c"));
        // A `..` that escapes is kept, so the caller can see it was refused
        // rather than find it silently clamped.
        assert_eq!(normalize(Path::new("../a")), PathBuf::from("../a"));
    }

    #[test]
    fn a_unified_diff_shows_both_sides_of_the_change() {
        let outcome = EditOutcome {
            path: PathBuf::from("src/lib.rs"),
            replacements: 1,
            before: "one\ntwo\nthree".to_owned(),
            after: "one\nTWO\nthree".to_owned(),
        };
        let diff = outcome.unified_diff();
        assert!(diff.contains("--- a/src/lib.rs"));
        assert!(diff.contains("+++ b/src/lib.rs"));
        assert!(diff.contains("@@ -2,1 +2,1 @@"));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+TWO"));
        assert!(!diff.contains("-one"), "unchanged lines are not re-printed");
    }

    #[test]
    fn a_unified_diff_of_an_unchanged_edit_is_empty() {
        let outcome = EditOutcome {
            path: PathBuf::from("a"),
            replacements: 0,
            before: "same".to_owned(),
            after: "same".to_owned(),
        };
        assert!(outcome.unified_diff().is_empty());
        assert!(outcome.is_empty());
    }

    #[test]
    fn a_unified_diff_handles_pure_insertion_and_deletion() {
        let added = EditOutcome {
            path: PathBuf::from("a"),
            replacements: 1,
            before: "one\nthree".to_owned(),
            after: "one\ntwo\nthree".to_owned(),
        };
        let diff = added.unified_diff();
        assert!(diff.contains("+two"));
        assert!(diff.contains("@@ -2,0 +2,1 @@"));

        let removed = EditOutcome {
            path: PathBuf::from("a"),
            replacements: 1,
            before: "one\ntwo\nthree".to_owned(),
            after: "one\nthree".to_owned(),
        };
        let diff = removed.unified_diff();
        assert!(diff.contains("-two"));
        assert!(diff.contains("@@ -2,1 +2,0 @@"));
    }

    #[test]
    fn search_queries_default_to_bounded_work() {
        let glob = SearchQuery::glob("/work", "**/*.rs");
        assert_eq!(glob.kind, SearchKind::Glob);
        assert_eq!(glob.max_results, SearchQuery::DEFAULT_MAX_RESULTS);
        assert_eq!(glob.max_file_bytes, SearchQuery::DEFAULT_MAX_FILE_BYTES);
        assert!(
            glob.case_sensitive,
            "searching is exact unless asked otherwise"
        );
        assert!(!glob.include_hidden);
        let literal = SearchQuery::literal("/work", "TODO")
            .with_max_results(5)
            .with_case_insensitive()
            .with_hidden();
        assert_eq!(literal.kind, SearchKind::Literal);
        assert_eq!(literal.max_results, 5);
        assert!(!literal.case_sensitive);
        assert!(literal.include_hidden);
    }
}
