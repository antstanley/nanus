//! Where a link's socket lives.
//!
//! Path arithmetic and nothing else, so where a socket *would* be is a value a test can
//! assert on without creating one.
//!
//! ## Why under the nanus home
//!
//! A Unix socket path is bounded (about 104 bytes on macOS), so a temporary directory
//! whose name is already a hundred characters is not a candidate: `/var/folders/…/T/`
//! is per-user on macOS and routinely too long to append a name to. The nanus home is
//! short, stable, and somewhere the user already expects nanus to keep state.
//!
//! ## Why two names
//!
//! A shell-scoped agent is one process among many, so its socket is named after the
//! process that owns it; a service is a singleton, so its socket has one name and a
//! second `nanus service start` is a collision rather than a second agent.

use std::path::{Path, PathBuf};

/// The directory, under the nanus home, that holds run-time sockets.
pub const RUN_DIR: &str = "run";

/// The file name of the long-running service's socket.
pub const SERVICE_SOCKET: &str = "agent.sock";

/// Returns the directory sockets live in.
#[must_use]
pub fn run_dir(home: &Path) -> PathBuf {
    home.join(RUN_DIR)
}

/// Returns the path the long-running service listens on.
#[must_use]
pub fn service_socket(home: &Path) -> PathBuf {
    run_dir(home).join(SERVICE_SOCKET)
}

/// Returns the path a shell-scoped agent owned by `pid` listens on.
///
/// Named after the process rather than chosen randomly so that a socket left behind by
/// a crash can be attributed to the run that made it.
#[must_use]
pub fn attached_socket(home: &Path, pid: u32) -> PathBuf {
    run_dir(home).join(format!("attach-{pid}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_socket_is_one_stable_name_under_the_home() {
        let home = Path::new("/home/stan/.config/nanus");
        assert_eq!(
            service_socket(home),
            PathBuf::from("/home/stan/.config/nanus/run/agent.sock")
        );
        // Stability is the property that matters: a client computes this path and a
        // server binds it, and the two never exchange it.
        assert_eq!(service_socket(home), service_socket(home));
    }

    #[test]
    fn an_attached_socket_is_named_after_its_process() {
        let home = Path::new("/home/stan/.config/nanus");
        let first = attached_socket(home, 41);
        let second = attached_socket(home, 42);
        assert_ne!(first, second, "two runs must not collide");
        assert_eq!(
            first,
            PathBuf::from("/home/stan/.config/nanus/run/attach-41.sock")
        );
        // Both live beside the service socket, which is what lets one directory hold
        // every live agent on the machine.
        assert_eq!(first.parent(), service_socket(home).parent());
    }
}
