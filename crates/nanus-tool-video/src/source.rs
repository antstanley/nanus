//! The rooted source: a workspace file, copied once under a size ceiling.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nanus_ports::{FsHandle, LocalBoxFuture};

use crate::VideoError;
use crate::media::{Snapshot, VideoSource};

/// The largest source the extension will copy (128 MiB).
pub const SNAPSHOT_BYTES_MAX: u64 = 128 * 1024 * 1024;

/// Reads sources through the filesystem port, so the workspace root confines them.
///
/// The port is what refuses a path outside the workspace; this type adds the size ceiling,
/// which is checked before the read and again on the bytes that arrived.
pub struct FsSource {
    fs: FsHandle,
    temp_root: Option<PathBuf>,
}

impl FsSource {
    /// Wraps the filesystem handle the stock tools use.
    #[must_use]
    pub const fn new(fs: FsHandle) -> Self {
        Self {
            fs,
            temp_root: None,
        }
    }

    /// Puts the snapshot directories under `root` rather than the system temporary directory.
    #[must_use]
    pub fn with_temp_root(mut self, root: PathBuf) -> Self {
        self.temp_root = Some(root);
        self
    }
}

impl VideoSource for FsSource {
    fn snapshot<'a>(&'a self, path: &'a str) -> LocalBoxFuture<'a, Result<Snapshot, VideoError>> {
        Box::pin(async move {
            let path = Path::new(path);
            let source =
                |error: &dyn std::fmt::Display| VideoError::Source(format!("read_video: {error}"));
            let meta = self
                .fs
                .metadata(path)
                .await
                .map_err(|error| source(&error))?;
            if !meta.is_file {
                return Err(VideoError::Source(
                    "read_video: file_path is not a regular file".to_owned(),
                ));
            }
            if meta.byte_len > SNAPSHOT_BYTES_MAX {
                return Err(VideoError::Source(format!(
                    "read_video: the file is {} bytes, above the {SNAPSHOT_BYTES_MAX}-byte ceiling",
                    meta.byte_len
                )));
            }
            let bytes = self
                .fs
                .read_bytes(path)
                .await
                .map_err(|error| source(&error))?;
            let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if byte_len > SNAPSHOT_BYTES_MAX {
                return Err(VideoError::Source(
                    "read_video: the file grew past its ceiling while it was read".to_owned(),
                ));
            }
            let digest = hex(blake3::hash(&bytes).as_bytes());
            let mut builder = tempfile::Builder::new();
            builder.prefix("nanus-video-");
            let directory = self
                .temp_root
                .as_ref()
                .map_or_else(|| builder.tempdir(), |root| builder.tempdir_in(root))
                .map_err(|error| source(&error))?;
            // A fixed name with no extension: nothing the model wrote reaches a command line,
            // and the container is decided by content rather than by a suffix.
            let copy = directory.path().join("source");
            tokio::fs::write(&copy, &bytes)
                .await
                .map_err(|error| source(&error))?;
            Ok(Snapshot {
                path: copy,
                blake3: digest,
                byte_len,
                owner: Arc::new(directory),
            })
        })
    }
}

/// Lowercase hex of a digest.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            text.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
            text
        })
}
