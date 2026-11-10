//! Unix domain sockets with owner-only filesystem permissions, and a peer that is checked.
//!
//! ## Two defences, because a socket can be put anywhere
//!
//! The default socket lives in a `0700` directory under the nanus home, and there the
//! directory is the whole of it: a peer that reached the socket already is the user. But
//! `--socket` and `service_socket` take any path, and a path in a directory other accounts can
//! write to is one another account can be listening at first — an interface would then hand
//! its prompts, and a credential typed into it, to somebody else's process. So both ends also
//! ask the operating system who the connection's peer is ([`require_same_user`]) and refuse
//! anyone who is not this user. The answer is the kernel's, not the peer's, which is why it
//! needs no exchange of the kind a Windows pipe does.

use crate::{LinkError, LinkResult};
use nix::sys::stat::{Mode, umask};
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::OnceLock;
use tokio::net::{UnixListener, UnixStream};

/// The permission bits that let somebody other than the owner write to a directory.
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// Binds a listener at `path`, creating the run directory if it is missing.
///
/// A socket file left behind by a process that died cannot be bound over, and cannot be
/// told from a live one by looking at it, so the only honest test is to ask it. The
/// permissions are narrowed to the owner: a socket that any local user can connect to is
/// a socket that any local user can drive an agent through.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when the directory cannot be created, something that is not a
/// socket is at the path, the bind fails, or the permissions cannot be set.
pub async fn bind(path: &Path) -> LinkResult<UnixListener> {
    let Some(parent) = path.parent() else {
        return Err(LinkError::protocol(format!(
            "{} has no directory to bind in",
            path.display()
        )));
    };
    create_run_dir(parent)?;
    clear_stale(path).await?;
    let listener = bind_private(path)?;
    restrict(path)?;
    Ok(listener)
}

/// Removes a socket nothing is listening on, and refuses anything else that is in the way.
///
/// Only a *socket* is ever removed. The check used to be "something is there and does not
/// answer", which is also true of a file: a `--socket` mistyped as the path of something worth
/// keeping deleted it. A link is refused as well rather than followed, because what it points
/// at is not this process's to replace.
async fn clear_stale(path: &Path) -> LinkResult<()> {
    let found = match std::fs::symlink_metadata(path) {
        Ok(found) => found,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !found.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket; it is left alone rather than replaced",
                path.display()
            ),
        )
        .into());
    }
    if UnixStream::connect(path).await.is_err() {
        let removed = tokio::fs::remove_file(path).await;
        if let Err(error) = removed {
            tracing::debug!(%error, "a stale socket could not be cleared");
        }
    }
    Ok(())
}

/// The mask a socket is bound under: nothing for the group or for anyone else.
///
/// It clears the group's and others' bits and *only* those. The owner's execute bit means
/// nothing on a socket, and [`restrict`] takes it off a moment later, but the mask is the
/// process's, not the thread's, so a directory some other thread creates while it is in force
/// gets it too — and a directory its owner cannot search is one nothing can be created in. An
/// earlier `0o177` did exactly that to every directory a concurrent thread made.
const BIND_MASK: Mode = Mode::S_IRWXG.union(Mode::S_IRWXO);

/// Serialises every change this module makes to the process's mask.
///
/// Reading the mask means setting it, so two binds that overlapped would each save the other's
/// tightened mask as "previous", and the last to put it back would leave it tightened for the
/// life of the process. Under this lock, every bind puts back the mask it found.
static MASK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Binds with the socket withheld from other accounts from the instant it exists.
///
/// A socket is created with the process's mask applied, and narrowing it afterwards leaves a
/// moment in which a permissive mask has made it connectable by others. The mask is tightened
/// for the bind alone and put back at once. It is the process's, not the thread's: a file some
/// other thread creates in that instant is withheld from the group and from others, which is
/// the harmless direction because [`BIND_MASK`] leaves the owner every bit, and nothing here
/// awaits while it is in force.
fn bind_private(path: &Path) -> std::io::Result<UnixListener> {
    let _held = lock_mask();
    bind_masked(path)
}

/// The bind itself, for a caller that already holds [`MASK`].
fn bind_masked(path: &Path) -> std::io::Result<UnixListener> {
    let previous = umask(BIND_MASK);
    let bound = UnixListener::bind(path);
    let _tightened = umask(previous);
    bound
}

/// Takes [`MASK`]. It guards no data, so a thread that panicked holding it left nothing torn.
fn lock_mask() -> std::sync::MutexGuard<'static, ()> {
    MASK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Creates the run directory with owner-only permissions, or looks at the one that is there.
///
/// A directory this creates is `0700`. One that already exists is used as it is — it may be
/// the user's own choice of where a second socket lives — but one that other accounts can write
/// to is said so in the log: the socket in it is still owner-only and its peers are still
/// checked, and another account can still be at the path before the agent is.
fn create_run_dir(path: &Path) -> LinkResult<()> {
    if let Ok(found) = std::fs::metadata(path)
        && found.is_dir()
    {
        if is_shared(found.permissions().mode()) {
            tracing::warn!(
                directory = %path.display(),
                "the socket's directory can be written by other accounts; \
                 keep an agent's socket in a directory that is yours"
            );
        }
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}

/// Whether a directory with these permission bits can be written by somebody other than its
/// owner.
const fn is_shared(mode: u32) -> bool {
    mode & WRITABLE_BY_OTHERS != 0
}

/// Narrows a socket's permissions to its owner.
fn restrict(path: &Path) -> LinkResult<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Requires the process at the other end of `stream` to be running as this user.
///
/// # Errors
///
/// Returns `PermissionDenied` naming both users when the peer is somebody else, and the
/// operating system's error when it will not say who the peer is — which is refused too: an
/// unknown peer is not a known one.
pub(super) fn require_same_user(stream: &UnixStream) -> std::io::Result<()> {
    same_user(stream.peer_cred()?.uid(), own_uid()?)
}

/// Compares a peer's user with this process's.
fn same_user(peer: u32, own: u32) -> std::io::Result<()> {
    if peer == own {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("the peer runs as user {peer} and this process as user {own}"),
    ))
}

/// The user this process runs as, as the kernel reports it for a socket's peer.
///
/// Read from the far end of a pair this process holds both ends of, so it is the same kind of
/// answer a real peer's is compared with, from the same call, and needs nothing that is not
/// safe to ask for. Asked once.
fn own_uid() -> std::io::Result<u32> {
    static OWN: OnceLock<u32> = OnceLock::new();
    if let Some(uid) = OWN.get() {
        return Ok(*uid);
    }
    let (near, _far) = UnixStream::pair()?;
    let uid = near.peer_cred()?.uid();
    Ok(*OWN.get_or_init(|| uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_peer_running_as_this_user_is_accepted() {
        let (near, far) = UnixStream::pair().unwrap();
        assert!(require_same_user(&near).is_ok());
        assert!(require_same_user(&far).is_ok());
    }

    /// The direction a test cannot stage without a second account, so the comparison is
    /// checked by itself: any other user is refused, root included, and the refusal names both.
    #[tokio::test]
    async fn a_peer_running_as_anyone_else_is_refused() {
        let own = own_uid().unwrap();
        for stranger in [own.wrapping_add(1), 0_u32.wrapping_add(u32::from(own == 0))] {
            let refused = same_user(stranger, own);
            let Err(error) = refused else {
                panic!("user {stranger} was accepted as user {own}");
            };
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains(&stranger.to_string()), "{error}");
        }
    }

    #[test]
    fn a_directory_others_can_write_to_is_told_from_one_that_is_private() {
        for shared in [0o777, 0o1777, 0o775, 0o702] {
            assert!(is_shared(shared), "{shared:o}");
        }
        for private in [0o700, 0o755, 0o750, 0o500] {
            assert!(!is_shared(private), "{private:o}");
        }
    }

    /// The socket is owner-only from the bind itself, whatever the process's mask: with the
    /// most permissive mask there is, nothing has to be narrowed afterwards for it to be private.
    #[tokio::test]
    async fn a_socket_is_private_before_it_is_narrowed_even_under_an_open_mask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let held = lock_mask();
        let before = umask(Mode::empty());
        let bound = bind_masked(&path);
        let restored = umask(before);
        drop(held);
        assert!(bound.is_ok(), "{bound:?}");
        assert_eq!(restored, Mode::empty(), "the mask is put back as it was");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "socket mode {mode:o}");
    }

    /// The mask is the process's, so whatever another thread creates during a bind is created
    /// under it. The owner keeps every bit — a directory made then can still be worked in — and
    /// the group and others still get none.
    #[test]
    fn a_directory_made_while_a_socket_is_bound_is_still_its_owners_to_use() {
        let held = lock_mask();
        let previous = umask(BIND_MASK);
        let created = tempfile::tempdir();
        let _tightened = umask(previous);
        drop(held);
        let created = created.unwrap();
        let mode = std::fs::metadata(created.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o700, 0o700, "directory mode {mode:o}");
        assert_eq!(mode & 0o077, 0, "directory mode {mode:o}");
        let inside = created.path().join("sessions");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("session.jsonl"), "{}").unwrap();
    }

    /// Reading the mask means setting it, so binds that overlap must not save each other's
    /// tightened mask as the one to put back: however many run at once, the process's mask
    /// afterwards is the one it had before.
    #[test]
    fn overlapping_binds_put_back_the_mask_they_found() {
        let dir = tempfile::tempdir().unwrap();
        let found = {
            let _held = lock_mask();
            let found = umask(BIND_MASK);
            umask(found);
            found
        };
        let binds: Vec<_> = (0..16)
            .map(|index| {
                let path = dir.path().join(format!("agent-{index}.sock"));
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_io()
                        .build()
                        .unwrap();
                    runtime.block_on(async { bind_private(&path).map(drop) })
                })
            })
            .collect();
        for bind in binds {
            bind.join().unwrap().unwrap();
        }
        let _held = lock_mask();
        let after = umask(BIND_MASK);
        umask(after);
        assert_eq!(
            after, found,
            "the mask is {after:?}, not the {found:?} it was"
        );
    }

    #[tokio::test]
    async fn a_file_at_the_socket_path_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "worth keeping").unwrap();
        let bound = bind(&path).await;
        assert!(
            matches!(&bound, Err(LinkError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists),
            "{bound:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "worth keeping");
    }

    #[tokio::test]
    async fn a_link_at_the_socket_path_is_refused_and_what_it_names_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.sock");
        let dead = UnixListener::bind(&target).unwrap();
        drop(dead);
        let path = dir.path().join("agent.sock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let bound = bind(&path).await;
        assert!(bound.is_err(), "a link is not this process's to replace");
        assert!(target.exists(), "the socket it names is untouched");
        assert!(path.is_symlink(), "and so is the link");
    }

    /// The other direction: a socket nothing is listening on is still what it always was, a
    /// leftover to bind over, and a live one is still left to whoever is serving it.
    #[tokio::test]
    async fn a_dead_socket_is_replaced_and_a_live_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let dead = UnixListener::bind(&path).unwrap();
        drop(dead);
        let live = bind(&path).await;
        assert!(live.is_ok(), "{live:?}");
        let second = bind(&path).await;
        assert!(second.is_err(), "a socket somebody is serving is not taken");
        assert!(UnixStream::connect(&path).await.is_ok());
    }
}
