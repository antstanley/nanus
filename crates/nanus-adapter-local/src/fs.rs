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
//! ## A window is read through the handle that was checked
//!
//! Both checks above are about a *path*, and a path can be changed between the check and the
//! open: a file swapped for a link, or a directory for a link to somewhere else. A ranged read
//! therefore checks the handle it actually opened. The final component is opened without
//! following a link (`O_NOFOLLOW` on Unix, the reparse point itself on Windows), the canonical
//! path is resolved again *after* the open and must still be inside the root, and the handle
//! must be the same file as that in-root path — the same device and inode on Unix. A handle
//! that reached outside through a swapped directory fails the last two, because the file it
//! holds is not the one the root names. On Windows, where the standard library exposes no
//! stable file index, "the same file" is the strongest safe comparison it does expose: length,
//! modification and creation time.
//!
//! ## Operations are synchronous
//!
//! The port's methods return futures; the work inside is synchronous. A local
//! filesystem has no completion-based form for a tree scan, so the alternative
//! would be wrapping blocking syscalls in a future without making anything
//! concurrent.

use std::fs::{File, Metadata};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use nanus_domain::context::managed::Digest;
use nanus_ports::{
    DirEntry, EditOutcome, FileIdentity, FileMeta, FileRead, FsError, FsPort, FsResult,
    LocalBoxFuture, RANGE_READ_MAX_BYTES, RangeRead, SearchKind, SearchMatch, SearchOutcome,
    SearchQuery, WriteMode, WriteOutcome, check_edit_count, ensure_within, occurrence_count,
};

/// How many leading bytes are sniffed for a NUL when classifying a file as binary.
const BINARY_SNIFF: usize = 8 * 1024;

/// How many times a ranged read is tried before a file that keeps changing is reported.
///
/// One retry absorbs a write that happened to land during the read; a file that changes
/// under every attempt is being rewritten continuously, and saying so is more useful than a
/// window whose identity names a version the bytes may not belong to.
const RANGE_READ_ATTEMPTS: u8 = 2;

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
        let matcher = Matcher::for_query(query)?;
        let include = compile_include(query.include.as_deref())?;
        let mut outcome = SearchOutcome::default();
        // `truncated` is a claim about what was *dropped*, so the cap is checked where a
        // match is about to be added rather than at the top of a loop. Stopping as soon as
        // the cap filled up said "more than {cap} matches" whenever any file remained to
        // walk — including when none of the rest matched at all, which is a confident
        // falsehood a model cannot check.
        for file in Self::walk(&root, query.include_hidden) {
            // The include filter runs before a file is read, counted or capped: a file the
            // caller excluded is not in scope, so its matches must not be able to fill the
            // cap and push out the one the caller asked for.
            if !is_included(include.as_ref(), &root, &file) {
                continue;
            }
            let stop = match &matcher {
                Matcher::Glob { set } => collect_glob(set, &root, file, query, &mut outcome),
                Matcher::Literal(needle) => scan_literal(&file, needle, query, &mut outcome),
            };
            if stop {
                break;
            }
        }
        Ok(outcome)
    }

    /// Reads one bounded window of a file through a handle proven to be the confined file.
    ///
    /// The identity is taken from the handle on both sides of the read. When the two differ
    /// the file changed while it was read, and the bytes may belong to neither version, so
    /// the read is tried once more before the change is reported as an error.
    fn read_range_blocking(
        &self,
        path: &Path,
        offset: u64,
        max_bytes: usize,
    ) -> FsResult<RangeRead> {
        let resolved = self.resolve(path)?;
        let mut file = open_confined(&self.root, &resolved)?;
        let take = max_bytes.min(RANGE_READ_MAX_BYTES);
        let mut attempts = 0_u8;
        while attempts < RANGE_READ_ATTEMPTS {
            attempts = attempts.saturating_add(1);
            let before = identity_of(&file, &resolved)?;
            let bytes = read_window(&mut file, &resolved, offset, take)?;
            let after = identity_of(&file, &resolved)?;
            if before != after {
                continue;
            }
            assert!(
                bytes.len() <= take,
                "a window never exceeds what it asked for"
            );
            let read_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let eof = offset.saturating_add(read_len) >= after.len;
            let range_sha256 = Digest::of(&bytes);
            return Ok(RangeRead {
                path: resolved,
                offset,
                bytes,
                identity: after,
                range_sha256,
                eof,
            });
        }
        Err(FsError::Io {
            path: resolved,
            message: String::from(
                "the file changed while it was being read; read the window again",
            ),
        })
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

impl Matcher {
    /// Compiles the matcher a query's kind names.
    fn for_query(query: &SearchQuery) -> FsResult<Self> {
        match query.kind {
            SearchKind::Glob => Ok(Self::Glob {
                set: compile_glob(&query.pattern)?,
            }),
            SearchKind::Literal => Ok(Self::Literal(prepare_literal(
                &query.pattern,
                query.case_sensitive,
            ))),
        }
    }
}

/// Records one file a glob search matched, returning whether the cap dropped it.
fn collect_glob(
    set: &GlobSet,
    root: &Path,
    file: PathBuf,
    query: &SearchQuery,
    outcome: &mut SearchOutcome,
) -> bool {
    // Matched against the path *relative to the search root*, so the pattern anchors
    // where the caller asked it to.
    let relative = file.strip_prefix(root).unwrap_or(&file);
    if !matches_glob(set, relative) {
        return false;
    }
    if outcome.matches.len() >= query.max_results {
        outcome.truncated = true;
        return true;
    }
    outcome.matches.push(SearchMatch {
        path: file,
        line_number: 0,
        line: String::new(),
    });
    false
}

/// Searches one file's text, counting it as skipped when it cannot be searched.
///
/// Returns whether the cap dropped a match. A skip is counted by its reason rather than
/// silently, because each skipped file is a place the result cannot speak for.
fn scan_literal(
    file: &Path,
    needle: &str,
    query: &SearchQuery,
    outcome: &mut SearchOutcome,
) -> bool {
    if metadata_len(file).is_some_and(|len| len > query.max_file_bytes) {
        outcome.skipped_large = outcome.skipped_large.saturating_add(1);
        return false;
    }
    let text = match text_for_search(file) {
        SearchText::Text(text) => text,
        SearchText::Binary => {
            outcome.skipped_binary = outcome.skipped_binary.saturating_add(1);
            return false;
        }
        SearchText::Unreadable => {
            outcome.skipped_unreadable = outcome.skipped_unreadable.saturating_add(1);
            return false;
        }
    };
    outcome.files_scanned = outcome.files_scanned.saturating_add(1);
    collect_literal(file, &text, needle, query, outcome)
}

/// Compiles a search's `include` filter, when it has one.
///
/// Unlike the search glob this one is not separator-anchored: it is a file filter in the
/// `grep --include` sense, where `*.rs` means every Rust file at any depth, and that is what
/// the tool that offers it has always promised.
fn compile_include(include: Option<&str>) -> FsResult<Option<GlobMatcher>> {
    let Some(include) = include else {
        return Ok(None);
    };
    let glob = GlobBuilder::new(include)
        .build()
        .map_err(|error| FsError::InvalidPattern {
            pattern: include.to_owned(),
            reason: error.to_string(),
        })?;
    Ok(Some(glob.compile_matcher()))
}

/// Whether `file` passes the include filter: by its path below the root, its full path, or
/// its name.
fn is_included(include: Option<&GlobMatcher>, root: &Path, file: &Path) -> bool {
    include.is_none_or(|matcher| {
        let relative = file.strip_prefix(root).unwrap_or(file);
        matcher.is_match(relative)
            || matcher.is_match(file)
            || file
                .file_name()
                .is_some_and(|name| matcher.is_match(Path::new(name)))
    })
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

/// What a content search made of one file.
enum SearchText {
    /// The file's text.
    Text(String),
    /// A NUL in the leading bytes, or bytes that are not UTF-8.
    Binary,
    /// The file could not be read at all.
    Unreadable,
}

/// Reads a file for content search, classifying the ones that cannot be searched.
fn text_for_search(path: &Path) -> SearchText {
    let Ok(bytes) = std::fs::read(path) else {
        return SearchText::Unreadable;
    };
    let sniff = bytes.get(..BINARY_SNIFF).unwrap_or(&bytes);
    if sniff.contains(&0) {
        return SearchText::Binary;
    }
    String::from_utf8(bytes).map_or(SearchText::Binary, SearchText::Text)
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

    fn read_range<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
        max_bytes: usize,
    ) -> LocalBoxFuture<'a, FsResult<RangeRead>> {
        Box::pin(async move { self.read_range_blocking(path, offset, max_bytes) })
    }
}

/// Opens a confined path for reading and proves the handle is the file the root names.
///
/// `resolved` has already passed [`LocalFs::resolve`], so it is canonical and inside `root`;
/// what this adds is the check on the *handle*. The path is classified first so a directory
/// or a device is refused the same way on every platform, the final component is opened
/// without following a link, and then the path is canonicalized again and compared with the
/// handle. A link swapped in for the file is refused by the open; a directory swapped for a
/// link to elsewhere is refused because the re-resolved path leaves the root or names a
/// different file from the one the handle holds.
fn open_confined(root: &Path, resolved: &Path) -> FsResult<File> {
    assert!(root.is_absolute(), "a confinement root is absolute");
    let outside = || FsError::OutsideWorkspace {
        root: root.to_path_buf(),
        path: resolved.to_path_buf(),
    };
    let named =
        std::fs::symlink_metadata(resolved).map_err(|source| io_error(resolved, &source))?;
    if named.file_type().is_symlink() {
        // A canonical path has no link in its last component, so one here was put there
        // after the path was resolved.
        return Err(outside());
    }
    classify_file(&named, resolved)?;
    let file = open_no_follow(resolved).map_err(|source| {
        let swapped = std::fs::symlink_metadata(resolved)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if swapped {
            outside()
        } else {
            io_error(resolved, &source)
        }
    })?;
    let handle = file
        .metadata()
        .map_err(|source| io_error(resolved, &source))?;
    if handle.file_type().is_symlink() {
        return Err(outside());
    }
    classify_file(&handle, resolved)?;
    let again = resolved
        .canonicalize()
        .map_err(|source| io_error(resolved, &source))?;
    if !again.starts_with(root) {
        return Err(outside());
    }
    let in_root = std::fs::metadata(&again).map_err(|source| io_error(&again, &source))?;
    if !same_file(&handle, &in_root) {
        return Err(outside());
    }
    assert!(handle.is_file(), "a confined handle is a regular file");
    Ok(file)
}

/// Refuses a directory and anything else that is not a regular file.
fn classify_file(metadata: &Metadata, path: &Path) -> FsResult<()> {
    if metadata.is_dir() {
        return Err(FsError::IsADirectory {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(FsError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Opens the final component itself rather than whatever a link there points at.
///
/// `O_NONBLOCK` is there for a FIFO swapped in after the path was classified: opening one
/// for reading would otherwise wait for a writer that may never come, and the handle check
/// that follows refuses it as not a file. On a regular file the flag changes nothing.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use nix::fcntl::OFlag;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
}

/// Opens the final component itself rather than whatever a link there points at.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` makes a link the thing opened, so the handle reports it as
/// a link and [`open_confined`] refuses it rather than reading through it.
#[cfg(windows)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`, from the Win32 API.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens a path for reading, on a platform with no way to refuse a link at open time.
#[cfg(not(any(unix, windows)))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// Whether two metadata records describe the same file.
#[cfg(unix)]
fn same_file(handle: &Metadata, named: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    handle.dev() == named.dev() && handle.ino() == named.ino()
}

/// Whether two metadata records describe the same file, as far as stable `std` can tell.
///
/// Without a stable file index the comparison is length, modification and creation time:
/// enough to refuse a handle that reached a different file, not a proof of sameness.
#[cfg(not(unix))]
fn same_file(handle: &Metadata, named: &Metadata) -> bool {
    handle.len() == named.len()
        && handle.modified().ok() == named.modified().ok()
        && handle.created().ok() == named.created().ok()
}

/// Reads up to `take` bytes from `offset`.
fn read_window(file: &mut File, path: &Path, offset: u64, take: usize) -> FsResult<Vec<u8>> {
    assert!(
        take <= RANGE_READ_MAX_BYTES,
        "a window is bounded before it is read"
    );
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| io_error(path, &source))?;
    let mut bytes = Vec::with_capacity(take);
    let limit = u64::try_from(take).unwrap_or(u64::MAX);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, &source))?;
    assert!(bytes.len() <= take, "a bounded read stays bounded");
    Ok(bytes)
}

/// The identity of the file a handle holds, read from the handle rather than the path.
fn identity_of(file: &File, path: &Path) -> FsResult<FileIdentity> {
    let metadata = file.metadata().map_err(|source| io_error(path, &source))?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos());
    // The device and inode, which survive a rename and change when a file is replaced. No
    // stable file id is exposed off Unix, and an absent one is reported as absent.
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt as _;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let file_id = None;
    Ok(FileIdentity {
        len: metadata.len(),
        modified_ns,
        file_id,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical workspace root and a canonical directory outside it, holding `secret.txt`.
    fn root_and_outside() -> (tempfile::TempDir, PathBuf, tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("seed");
        let root_path = root.path().canonicalize().expect("canonical root");
        let outside_path = outside.path().canonicalize().expect("canonical outside");
        (root, root_path, outside, outside_path)
    }

    #[test]
    fn a_handle_on_a_file_inside_the_root_is_accepted() {
        let (_root, root_path, _outside, _outside_path) = root_and_outside();
        let inside = root_path.join("inside.txt");
        std::fs::write(&inside, "inside").expect("seed");
        let mut file = open_confined(&root_path, &inside).expect("an in-root file opens");
        let mut text = String::new();
        file.read_to_string(&mut text).expect("read");
        assert_eq!(text, "inside");
        // The other direction: the same check refuses a directory, which is not a window.
        let refused = open_confined(&root_path, &root_path);
        assert!(
            matches!(refused, Err(FsError::IsADirectory { .. })),
            "{refused:?}"
        );
    }

    /// The path passed in is what `resolve` would have produced *before* the swap: canonical
    /// and inside. A link put in its place afterwards must not be followed out of the root.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_link_after_resolution_is_refused() {
        let (_root, root_path, _outside, outside_path) = root_and_outside();
        let swapped = root_path.join("swapped.txt");
        std::os::unix::fs::symlink(outside_path.join("secret.txt"), &swapped).expect("link");
        let refused = open_confined(&root_path, &swapped);
        assert!(
            matches!(refused, Err(FsError::OutsideWorkspace { .. })),
            "{refused:?}"
        );
    }

    /// A directory swapped for a link reaches past `O_NOFOLLOW`, which guards only the last
    /// component; the re-resolution after the open is what refuses it.
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_link_after_resolution_is_refused() {
        let (_root, root_path, _outside, outside_path) = root_and_outside();
        std::os::unix::fs::symlink(&outside_path, root_path.join("dir")).expect("link");
        let through = root_path.join("dir").join("secret.txt");
        assert!(
            through.exists(),
            "the swapped path does reach the outside file"
        );
        let refused = open_confined(&root_path, &through);
        assert!(
            matches!(refused, Err(FsError::OutsideWorkspace { .. })),
            "{refused:?}"
        );
    }

    #[test]
    fn two_handles_on_one_file_are_the_same_file_and_two_files_are_not() {
        let (_root, root_path, _outside, outside_path) = root_and_outside();
        let inside = root_path.join("inside.txt");
        std::fs::write(&inside, "secret").expect("seed");
        let first = File::open(&inside)
            .expect("open")
            .metadata()
            .expect("metadata");
        let second = std::fs::metadata(&inside).expect("metadata");
        assert!(same_file(&first, &second));
        // Same length and contents, different file: the case a handle check exists for.
        let other = std::fs::metadata(outside_path.join("secret.txt")).expect("metadata");
        #[cfg(unix)]
        assert!(!same_file(&first, &other));
        #[cfg(not(unix))]
        let _ = other;
    }
}
