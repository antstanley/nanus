//! The rooted local filesystem adapter: an implementation of
//! [`nanus_ports::FsPort`].
//!
//! Every path is resolved against a workspace root, and a path that escapes it —
//! by `..`, by being absolute, or through a symlink whose target lies outside — is
//! rejected with [`FsError::OutsideWorkspace`]. Confinement uses the port's own
//! [`ensure_within`], so a consumer and this adapter cannot disagree about what
//! "inside the workspace" means.
//!
//! ## Two checks, not one
//!
//! [`ensure_within`] is lexical: it resolves `.` and `..` with no disk access, so
//! it works for a path that does not exist yet. That is necessary but not
//! sufficient, because a *symlink* inside the workspace can point outside it.
//! Every operation therefore also canonicalizes: the longest existing ancestor is
//! resolved through the filesystem and re-checked against the root. Both checks
//! must pass.
//!
//! ## Operations are synchronous
//!
//! The port's methods return futures; the work inside is synchronous. A local
//! filesystem has no completion-based form for a tree scan, so the alternative
//! would be wrapping blocking syscalls in a future without making anything
//! concurrent.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use nanus_ports::{
    DirEntry, EditOutcome, FileMeta, FileRead, FsError, FsPort, FsResult, LocalBoxFuture,
    SearchKind, SearchMatch, SearchOutcome, SearchQuery, WriteMode, WriteOutcome, check_edit_count,
    ensure_within, occurrence_count,
};

/// How many leading bytes are sniffed for a NUL when classifying a file as binary.
const BINARY_SNIFF: usize = 8 * 1024;

/// Directory names that hold version-control metadata and are never searched.
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".bzr"];

/// A filesystem adapter rooted at a workspace directory.
#[derive(Debug, Clone)]
pub struct LocalFs {
    /// The canonical workspace root.
    root: PathBuf,
    /// Whether a write may create missing parent directories.
    ///
    /// Off by default: creating directories is a side effect a caller must ask
    /// for. The port's [`WriteMode`] says whether an existing file may be
    /// replaced, which is a different question, so this is adapter policy rather
    /// than part of the request.
    create_parents: bool,
}

impl LocalFs {
    /// Roots an adapter at `root`, canonicalizing it once.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::Io`] when the root cannot be canonicalized.
    pub fn new(root: impl Into<PathBuf>) -> FsResult<Self> {
        let requested = root.into();
        let canonical = requested
            .canonicalize()
            .map_err(|source| io_error(&requested, &source))?;
        assert!(canonical.is_absolute(), "a canonical root is absolute");
        Ok(Self {
            root: canonical,
            create_parents: false,
        })
    }

    /// Returns the same adapter with parent-directory creation enabled.
    #[must_use]
    pub const fn with_parent_creation(mut self, create_parents: bool) -> Self {
        self.create_parents = create_parents;
        self
    }

    /// Returns the canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Shares this adapter as the handle a kernel plugin publishes.
    #[must_use]
    pub fn handle(self) -> nanus_ports::FsHandle {
        std::rc::Rc::new(Box::new(self))
    }

    /// Resolves `path` lexically and through the filesystem, rejecting an escape.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::OutsideWorkspace`] when either check fails.
    fn resolve(&self, path: &Path) -> FsResult<PathBuf> {
        let lexical = ensure_within(&self.root, path)?;
        // The lexical check cannot see a symlink, so the longest existing
        // ancestor is canonicalized and re-checked.
        let mut existing = lexical.as_path();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let canonical_prefix = loop {
            if let Ok(canonical) = existing.canonicalize() {
                break canonical;
            }
            let Some(name) = existing.file_name() else {
                return Err(FsError::OutsideWorkspace {
                    root: self.root.clone(),
                    path: lexical,
                });
            };
            tail.push(name.to_os_string());
            let Some(parent) = existing.parent() else {
                return Err(FsError::OutsideWorkspace {
                    root: self.root.clone(),
                    path: lexical,
                });
            };
            existing = parent;
        };
        let mut resolved = canonical_prefix;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        if !resolved.starts_with(&self.root) {
            return Err(FsError::OutsideWorkspace {
                root: self.root.clone(),
                path: resolved,
            });
        }
        assert!(
            resolved.starts_with(&self.root),
            "a resolved path stays inside the root"
        );
        Ok(resolved)
    }

    /// Reads a file and counts its lines.
    fn read_blocking(&self, path: &Path) -> FsResult<FileRead> {
        let resolved = self.resolve(path)?;
        let bytes = read_checked(&resolved)?;
        let text = String::from_utf8(bytes).map_err(|_| FsError::Io {
            path: resolved.clone(),
            message: String::from("the file is not valid UTF-8"),
        })?;
        let total_lines = text.lines().count();
        Ok(FileRead {
            path: resolved,
            text,
            total_lines,
        })
    }

    /// Writes a file under the given mode.
    fn write_blocking(
        &self,
        path: &Path,
        contents: &str,
        mode: WriteMode,
    ) -> FsResult<WriteOutcome> {
        use std::io::Write as _;
        let resolved = self.resolve(path)?;
        let existed = resolved.exists();
        // Parents first: opening for creation in a missing directory would fail
        // before the directory could be made.
        if self.create_parents
            && let Some(parent) = resolved.parent()
        {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, &source))?;
        }
        let created = match mode {
            WriteMode::Create => {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&resolved)
                    .map_err(|source| {
                        if source.kind() == std::io::ErrorKind::AlreadyExists {
                            FsError::AlreadyExists {
                                path: resolved.clone(),
                            }
                        } else {
                            io_error(&resolved, &source)
                        }
                    })?;
                file.write_all(contents.as_bytes())
                    .map_err(|source| io_error(&resolved, &source))?;
                true
            }
            WriteMode::Overwrite => {
                std::fs::write(&resolved, contents)
                    .map_err(|source| io_error(&resolved, &source))?;
                !existed
            }
        };
        let bytes_written = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        Ok(WriteOutcome {
            path: resolved,
            bytes_written,
            created,
        })
    }

    /// Replaces text in a file under the exactly-once rule.
    fn edit_blocking(
        &self,
        path: &Path,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> FsResult<EditOutcome> {
        let resolved = self.resolve(path)?;
        let before = self.read_blocking(path)?.text;
        let count = occurrence_count(&before, old);
        let replacements = check_edit_count(&resolved, count, replace_all)?;
        let after = if replace_all {
            before.replace(old, new)
        } else {
            before.replacen(old, new, 1)
        };
        if old != new {
            assert_ne!(before, after, "a successful edit changes the text");
        }
        std::fs::write(&resolved, &after).map_err(|source| io_error(&resolved, &source))?;
        Ok(EditOutcome {
            path: resolved,
            replacements,
            before,
            after,
        })
    }

    /// Lists a directory, one level deep.
    fn list_blocking(&self, dir: &Path) -> FsResult<Vec<DirEntry>> {
        let resolved = self.resolve(dir)?;
        let reader = std::fs::read_dir(&resolved).map_err(|source| io_error(&resolved, &source))?;
        let mut entries: Vec<DirEntry> = Vec::new();
        for entry in reader {
            let entry = entry.map_err(|source| io_error(&resolved, &source))?;
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|source| io_error(&entry.path(), &source))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            entries.push(DirEntry {
                path: entry.path(),
                name,
                is_dir: metadata.is_dir(),
                is_symlink: metadata.file_type().is_symlink(),
                byte_len: metadata.len(),
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    /// Walks the tree, excluding version-control metadata.
    fn walk(root: &Path, include_hidden: bool) -> Vec<PathBuf> {
        let walker = WalkBuilder::new(root)
            .hidden(!include_hidden)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .require_git(false)
            .filter_entry(|entry| !is_vcs_dir(entry.path()))
            .build();
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in walker.flatten() {
            // `flatten` drops per-entry errors: an unreadable corner of the tree
            // is skipped, not fatal to the search.
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(entry.into_path());
            }
        }
        files.sort_unstable();
        files
    }

    /// Searches by glob or by literal substring.
    fn search_blocking(&self, query: &SearchQuery) -> FsResult<SearchOutcome> {
        let root = self.resolve(&query.root)?;
        assert!(
            query.max_results > 0,
            "a search with a zero cap can never report anything"
        );
        let set = match query.kind {
            SearchKind::Glob => Some(compile_glob(&query.pattern)?),
            SearchKind::Literal => None,
        };
        let needle = match query.kind {
            SearchKind::Glob => None,
            SearchKind::Literal => Some(prepare_literal(&query.pattern, query.case_sensitive)),
        };
        let matcher = match (set, needle) {
            (Some(set), _) => Matcher::Glob { set },
            (None, Some(needle)) => Matcher::Literal(needle),
            (None, None) => {
                return Err(FsError::InvalidPattern {
                    pattern: query.pattern.clone(),
                    reason: String::from("a search kind selects exactly one matcher"),
                });
            }
        };
        let mut outcome = SearchOutcome {
            matches: Vec::new(),
            truncated: false,
            files_scanned: 0,
        };
        // `truncated` is a claim about what was *dropped*, so the cap is checked where a
        // match is about to be added rather than at the top of a loop. Stopping as soon as
        // the cap filled up said "more than {cap} matches" whenever any file remained to
        // walk — including when none of the rest matched at all, which is a confident
        // falsehood a model cannot check.
        'files: for file in Self::walk(&root, query.include_hidden) {
            match &matcher {
                Matcher::Glob { set } => {
                    // Matched against the path *relative to the search root*, so the
                    // pattern anchors where the caller asked it to.
                    let relative = file.strip_prefix(&root).unwrap_or(&file);
                    if !matches_glob(set, relative) {
                        continue;
                    }
                    if outcome.matches.len() >= query.max_results {
                        outcome.truncated = true;
                        break 'files;
                    }
                    outcome.matches.push(SearchMatch {
                        path: file,
                        line_number: 0,
                        line: String::new(),
                    });
                }
                Matcher::Literal(needle) => {
                    if let Some(len) = metadata_len(&file)
                        && len > query.max_file_bytes
                    {
                        continue;
                    }
                    let Some(text) = text_for_search(&file) else {
                        continue;
                    };
                    outcome.files_scanned = outcome.files_scanned.saturating_add(1);
                    if collect_literal(&file, &text, needle, query, &mut outcome) {
                        break 'files;
                    }
                }
            }
        }
        Ok(outcome)
    }
}

/// The matcher a search query selects.
enum Matcher {
    /// Match a path, relative to the search root, against a compiled glob.
    Glob {
        /// The compiled set.
        set: GlobSet,
    },
    /// Match file contents against a prepared literal.
    Literal(String),
}

/// Whether `relative` — a path relative to the search root — matches the glob.
///
/// There is deliberately no basename fallback. One used to exist so that a bare `*.rs`
/// matched by file name at any depth, which is convenient and wrong: it made the
/// anchoring meaningless, so `*.rs` and `**/*.rs` behaved identically and a model could
/// not express "just the top level". A caller that wants every depth writes `**/`.
fn matches_glob(set: &GlobSet, relative: &Path) -> bool {
    set.is_match(relative)
}

/// Compiles a glob into a set, anchored so `*` does not cross a directory separator.
///
/// Anchoring is the whole point. Under `globset`'s defaults `*` matches `/` as well, so
/// `*.rs` would match every `.rs` file at any depth — a tool named `glob` that silently
/// searched recursively. Worse, matching a pattern against an *absolute* path made every
/// bare pattern match, because the pattern's own separator-free prefix was satisfied by
/// the leading directories. With `literal_separator` set, `*.rs` means "a `.rs` file
/// directly in the search root" and `**/*.rs` means "at any depth", which is what every
/// reader of the pattern expects.
fn compile_glob(pattern: &str) -> FsResult<GlobSet> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| FsError::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: error.to_string(),
        })?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder.build().map_err(|error| FsError::InvalidPattern {
        pattern: pattern.to_owned(),
        reason: error.to_string(),
    })
}

/// Folds case when the query asks for it.
fn prepare_literal(pattern: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        pattern.to_owned()
    } else {
        pattern.to_lowercase()
    }
}

/// Returns a file's size, or `None` when it cannot be read.
fn metadata_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.len())
}

/// Reads a file for content search, returning `None` for binary files.
fn text_for_search(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let sniff = bytes.get(..BINARY_SNIFF).unwrap_or(&bytes);
    if sniff.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Appends the matching lines of one file, returning whether the cap dropped a match.
///
/// A `true` means a match was found beyond the cap and is *not* in the result, which is the
/// only thing that makes the search truncated. Finding exactly the cap's worth of matches and
/// then running out of text is a complete result, and reporting it as truncated would send a
/// model looking for something that is not there.
fn collect_literal(
    path: &Path,
    text: &str,
    needle: &str,
    query: &SearchQuery,
    outcome: &mut SearchOutcome,
) -> bool {
    for (index, line) in text.lines().enumerate() {
        let haystack = if query.case_sensitive {
            line.to_owned()
        } else {
            line.to_lowercase()
        };
        if !haystack.contains(needle) {
            continue;
        }
        if outcome.matches.len() >= query.max_results {
            outcome.truncated = true;
            return true;
        }
        outcome.matches.push(SearchMatch {
            path: path.to_path_buf(),
            line_number: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
            line: line.to_owned(),
        });
    }
    false
}

/// Whether `path` names a version-control metadata directory.
fn is_vcs_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| VCS_DIRS.contains(&name))
}

/// Translates an operating-system failure into the port's vocabulary.
fn io_error(path: &Path, source: &std::io::Error) -> FsError {
    match source.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound {
            path: path.to_path_buf(),
        },
        std::io::ErrorKind::PermissionDenied => FsError::Permission {
            path: path.to_path_buf(),
            message: source.to_string(),
        },
        _ => FsError::Io {
            path: path.to_path_buf(),
            message: source.to_string(),
        },
    }
}

impl FsPort for LocalFs {
    fn read<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<FileRead>> {
        Box::pin(async move { self.read_blocking(path) })
    }

    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
        mode: WriteMode,
    ) -> LocalBoxFuture<'a, FsResult<WriteOutcome>> {
        Box::pin(async move { self.write_blocking(path, contents, mode) })
    }

    fn edit<'a>(
        &'a self,
        path: &'a Path,
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    ) -> LocalBoxFuture<'a, FsResult<EditOutcome>> {
        Box::pin(async move { self.edit_blocking(path, old, new, replace_all) })
    }

    fn exists<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, bool> {
        // Infallible by contract: an escaping or unreadable path simply does not
        // exist as far as the caller is concerned.
        Box::pin(async move { self.resolve(path).is_ok_and(|resolved| resolved.exists()) })
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<FileMeta>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;
            let metadata = std::fs::symlink_metadata(&resolved)
                .map_err(|source| io_error(&resolved, &source))?;
            Ok(FileMeta {
                path: resolved,
                is_file: metadata.is_file(),
                is_dir: metadata.is_dir(),
                byte_len: metadata.len(),
            })
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> LocalBoxFuture<'a, FsResult<Vec<DirEntry>>> {
        Box::pin(async move { self.list_blocking(dir) })
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> LocalBoxFuture<'a, FsResult<SearchOutcome>> {
        Box::pin(async move { self.search_blocking(query) })
    }

    fn canonicalize<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move { self.resolve(path) })
    }

    fn read_bytes<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<Vec<u8>>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;
            read_checked(&resolved)
        })
    }
}

/// Reads a path the adapter has already confined, refusing anything that is not a file.
///
/// Shared by both reads so the two cannot disagree about what a readable path is: the
/// binary one exists for images, and an image tool that accepted a directory where the text
/// tool refused one would be a second, quieter definition of "readable".
fn read_checked(resolved: &Path) -> FsResult<Vec<u8>> {
    let metadata = std::fs::metadata(resolved).map_err(|source| io_error(resolved, &source))?;
    if metadata.is_dir() {
        return Err(FsError::IsADirectory {
            path: resolved.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(FsError::NotAFile {
            path: resolved.to_path_buf(),
        });
    }
    std::fs::read(resolved).map_err(|source| io_error(resolved, &source))
}
