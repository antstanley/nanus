//! Test doubles shared by the tool modules.
//!
//! Each tool's tests need a port that is never actually exercised — they check
//! argument handling, schema shape, and rendering, not I/O. One implementation here
//! is clearer than one per module, and it fails loudly if a test accidentally
//! depends on a real call.

use nanus_ports::{
    DirEntry, EditOutcome, FileMeta, FileRead, FsError, FsPort, FsResult, LocalBoxFuture,
    SearchOutcome, SearchQuery, WriteMode, WriteOutcome,
};

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
