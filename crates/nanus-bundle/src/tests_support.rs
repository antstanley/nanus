//! Test doubles shared by the tool modules.
//!
//! Each tool's tests need a port that is never actually exercised — they check
//! argument handling, schema shape, and rendering, not I/O. One implementation here
//! is clearer than one per module, and it fails loudly if a test accidentally
//! depends on a real call.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use nanus_ports::{
    DirEntry, EditOutcome, FileIdentity, FileMeta, FileRead, FsError, FsPort, FsResult,
    LocalBoxFuture, RANGE_READ_MAX_BYTES, RangeRead, SearchOutcome, SearchQuery, WriteMode,
    WriteOutcome,
};

/// A filesystem holding one file in memory, and a canned search answer.
///
/// The file answers to any path: the tools under test do not interpret paths, and
/// confinement is the adapter's to test rather than theirs. Replacing the bytes advances the
/// modification time, so a ranged read sees a new identity exactly as it would on disk.
/// Clones share one state, so a test can keep a clone to change the file after handing the
/// other to a tool.
#[derive(Clone)]
pub struct MemoryFs {
    state: Rc<MemoryState>,
}

/// What a [`MemoryFs`] and its clones share.
struct MemoryState {
    bytes: RefCell<Vec<u8>>,
    generation: Cell<u128>,
    search: RefCell<SearchOutcome>,
    queries: RefCell<Vec<SearchQuery>>,
}

impl MemoryFs {
    /// A filesystem whose one file holds `bytes`.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            state: Rc::new(MemoryState {
                bytes: RefCell::new(bytes.into()),
                generation: Cell::new(1),
                search: RefCell::new(SearchOutcome::default()),
                queries: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Replaces the file's bytes, as a rewrite on disk would.
    pub fn replace(&self, bytes: impl Into<Vec<u8>>) {
        *self.state.bytes.borrow_mut() = bytes.into();
        self.state
            .generation
            .set(self.state.generation.get().saturating_add(1));
    }

    /// Answers every search with `outcome`.
    pub fn answer_searches_with(self, outcome: SearchOutcome) -> Self {
        *self.state.search.borrow_mut() = outcome;
        self
    }

    /// The queries searched so far, in order.
    pub fn queries(&self) -> Vec<SearchQuery> {
        self.state.queries.borrow().clone()
    }

    /// Shares a clone as the handle a tool takes.
    pub fn handle(&self) -> nanus_ports::FsHandle {
        Rc::new(Box::new(self.clone()))
    }

    fn len(&self) -> u64 {
        u64::try_from(self.state.bytes.borrow().len()).unwrap_or(u64::MAX)
    }

    fn range(&self, path: &std::path::Path, offset: u64, max_bytes: usize) -> RangeRead {
        let bytes = self.state.bytes.borrow();
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let end = start
            .saturating_add(max_bytes.min(RANGE_READ_MAX_BYTES))
            .min(bytes.len());
        let window = bytes.get(start..end).unwrap_or_default().to_vec();
        RangeRead {
            path: path.to_path_buf(),
            offset,
            range_sha256: nanus_domain::context::managed::Digest::of(&window),
            eof: end == bytes.len(),
            bytes: window,
            identity: FileIdentity {
                len: self.len(),
                modified_ns: Some(self.state.generation.get()),
                file_id: Some(String::from("1:1")),
            },
        }
    }
}

impl FsPort for MemoryFs {
    fn read<'a>(&'a self, path: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<FileRead>> {
        let text = String::from_utf8(self.state.bytes.borrow().clone());
        Box::pin(async move {
            let text = text.map_err(|_| FsError::Io {
                path: path.to_path_buf(),
                message: String::from("the file is not valid UTF-8"),
            })?;
            Ok(FileRead {
                path: path.to_path_buf(),
                total_lines: text.lines().count(),
                text,
            })
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a std::path::Path,
        contents: &'a str,
        mode: WriteMode,
    ) -> LocalBoxFuture<'a, FsResult<WriteOutcome>> {
        let _ = (contents, mode);
        Box::pin(async move { UnusedFs::missing(path) })
    }

    fn edit<'a>(
        &'a self,
        path: &'a std::path::Path,
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    ) -> LocalBoxFuture<'a, FsResult<EditOutcome>> {
        let _ = (old, new, replace_all);
        Box::pin(async move { UnusedFs::missing(path) })
    }

    fn exists<'a>(&'a self, _path: &'a std::path::Path) -> LocalBoxFuture<'a, bool> {
        Box::pin(async { true })
    }

    fn metadata<'a>(&'a self, path: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<FileMeta>> {
        let byte_len = self.len();
        Box::pin(async move {
            Ok(FileMeta {
                path: path.to_path_buf(),
                is_file: true,
                is_dir: false,
                byte_len,
            })
        })
    }

    fn list<'a>(&'a self, dir: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<Vec<DirEntry>>> {
        Box::pin(async move { UnusedFs::missing(dir) })
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> LocalBoxFuture<'a, FsResult<SearchOutcome>> {
        self.state.queries.borrow_mut().push(query.clone());
        let outcome = self.state.search.borrow().clone();
        Box::pin(async move { Ok(outcome) })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a std::path::Path,
    ) -> LocalBoxFuture<'a, FsResult<std::path::PathBuf>> {
        Box::pin(async move { UnusedFs::missing(path) })
    }

    fn read_bytes<'a>(
        &'a self,
        _path: &'a std::path::Path,
    ) -> LocalBoxFuture<'a, FsResult<Vec<u8>>> {
        let bytes = self.state.bytes.borrow().clone();
        Box::pin(async move { Ok(bytes) })
    }

    fn read_range<'a>(
        &'a self,
        path: &'a std::path::Path,
        offset: u64,
        max_bytes: usize,
    ) -> LocalBoxFuture<'a, FsResult<RangeRead>> {
        let read = self.range(path, offset, max_bytes);
        Box::pin(async move { Ok(read) })
    }
}

/// A filesystem whose every call fails with "not found".
///
/// A tool given this port can still validate its arguments and render a failure, so
/// a test can assert on those paths without a temporary directory.
pub struct UnusedFs;

impl UnusedFs {
    fn missing<T>(path: &std::path::Path) -> FsResult<T> {
        Err(FsError::NotFound {
            path: path.to_path_buf(),
        })
    }
}

impl FsPort for UnusedFs {
    fn read<'a>(&'a self, path: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<FileRead>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }

    fn write<'a>(
        &'a self,
        path: &'a std::path::Path,
        _contents: &'a str,
        _mode: WriteMode,
    ) -> LocalBoxFuture<'a, FsResult<WriteOutcome>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }

    fn edit<'a>(
        &'a self,
        path: &'a std::path::Path,
        _old: &'a str,
        _new: &'a str,
        _replace_all: bool,
    ) -> LocalBoxFuture<'a, FsResult<EditOutcome>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }

    fn read_bytes<'a>(
        &'a self,
        path: &'a std::path::Path,
    ) -> LocalBoxFuture<'a, FsResult<Vec<u8>>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }

    fn exists<'a>(&'a self, _path: &'a std::path::Path) -> LocalBoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn metadata<'a>(&'a self, path: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<FileMeta>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }

    fn list<'a>(&'a self, dir: &'a std::path::Path) -> LocalBoxFuture<'a, FsResult<Vec<DirEntry>>> {
        let dir = dir.to_path_buf();
        Box::pin(async move { Self::missing(&dir) })
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> LocalBoxFuture<'a, FsResult<SearchOutcome>> {
        let root = query.root.clone();
        Box::pin(async move { Self::missing(&root) })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a std::path::Path,
    ) -> LocalBoxFuture<'a, FsResult<std::path::PathBuf>> {
        let path = path.to_path_buf();
        Box::pin(async move { Self::missing(&path) })
    }
}

/// A shell that is never run.
pub struct UnusedShell;

impl nanus_ports::ShellPort for UnusedShell {
    fn run(
        &self,
        request: nanus_ports::ShellRequest,
    ) -> LocalBoxFuture<'_, nanus_ports::ShellResult<nanus_ports::ShellOutcome>> {
        // The program is named in the error so a test that accidentally reaches here
        // can see which call it was.
        Box::pin(async move {
            Err(nanus_ports::ShellError::Spawn {
                program: request.program,
                message: "the test shell is never run".to_owned(),
            })
        })
    }

    fn spawn(
        &self,
        request: nanus_ports::ShellRequest,
    ) -> LocalBoxFuture<'_, nanus_ports::ShellResult<nanus_ports::ShellStream>> {
        Box::pin(async move {
            Err(nanus_ports::ShellError::Spawn {
                program: request.program,
                message: "the test shell is never run".to_owned(),
            })
        })
    }

    fn kill_all(&self) -> LocalBoxFuture<'_, nanus_ports::ShellResult<usize>> {
        Box::pin(async { Ok(0) })
    }

    fn sandbox(&self) -> nanus_ports::SandboxPolicy {
        nanus_ports::SandboxPolicy::read_only(std::env::temp_dir())
    }
}
