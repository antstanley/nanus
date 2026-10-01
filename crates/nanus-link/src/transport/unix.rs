//! Unix domain sockets with owner-only filesystem permissions.

use crate::{LinkError, LinkResult};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

/// Binds a listener at `path`, creating the run directory if it is missing.
///
/// A socket file left behind by a process that died cannot be bound over, and cannot be
/// told from a live one by looking at it, so the only honest test is to ask it. The
/// permissions are narrowed to the owner: a socket that any local user can connect to is
/// a socket that any local user can drive an agent through.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when the directory cannot be created, the bind fails, or
/// the permissions cannot be set.
pub async fn bind(path: &Path) -> LinkResult<UnixListener> {
    let Some(parent) = path.parent() else {
        return Err(LinkError::protocol(format!(
            "{} has no directory to bind in",
            path.display()
        )));
    };
    create_run_dir(parent)?;
    if path.exists() && UnixStream::connect(path).await.is_err() {
        let removed = tokio::fs::remove_file(path).await;
        if let Err(error) = removed {
            tracing::debug!(%error, "a stale socket could not be cleared");
        }
    }
    let listener = UnixListener::bind(path)?;
    restrict(path)?;
    Ok(listener)
}

/// Creates the run directory with owner-only permissions.
fn create_run_dir(path: &Path) -> LinkResult<()> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}

/// Narrows a socket's permissions to its owner.
fn restrict(path: &Path) -> LinkResult<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
