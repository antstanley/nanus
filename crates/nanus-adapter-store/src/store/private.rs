//! Owner-only directories and files.
//!
//! A transcript holds everything the model saw and produced, including any secret it read, and
//! the archive holds raw shell output. Left to the process's umask — commonly `022` — the store
//! was world-readable: `0755` directories and `0644` files, which on a shared machine is every
//! conversation readable by every other account. Everything the store creates under the home is
//! therefore created for its owner alone: directories `0700`, files `0600`, the same modes the
//! credential file store and the link's socket directory already use.
//!
//! ## A store that already exists
//!
//! Opening a store narrows the three directories everything else sits under — `sessions/`,
//! `retired/` and `trash/` — when they grant anything to group or other. That is one `chmod`
//! each rather than a walk, and it is enough: a file nobody can reach the directory of is not
//! readable, whatever its own mode says. A session directory is narrowed when it is next
//! claimed, and a log takes the new mode the next time it is written, since a write replaces
//! the file. Narrowing only ever removes access from *other* accounts, so a store that was
//! `0755` is exactly as readable to its owner afterwards. A directory this process may not
//! change — one owned by another account — is reported and left: the store still opens.
//!
//! The home itself is not narrowed when it already exists. It is the user's directory, it holds
//! the configuration file as well, and what the store keeps in it directly are two empty lock
//! files.
//!
//! ## Windows
//!
//! There are no mode bits to set: a file inherits its directory's access-control list, and this
//! module sets no explicit descriptor. A home under the user's profile is private by that
//! inheritance; one placed elsewhere is as private as the directory it was placed in.

use std::path::Path;

/// The mode of a directory the store creates.
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;

/// The mode of a file the store creates.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// The bits that grant anything to an account other than the owner.
#[cfg(unix)]
const OTHERS: u32 = 0o077;

/// Creates `dir` and any missing parent, each for its owner alone.
///
/// A directory that already exists keeps its mode; see [`narrow_dir`].
pub(super) async fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(DIR_MODE);
    builder.create(dir).await
}

/// [`create_dir_all`], for the callers that cannot yield.
pub(super) fn create_dir_all_blocking(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(DIR_MODE);
    }
    builder.create(dir)
}

/// Open options under which a file that gets created is its owner's alone.
///
/// The mode applies only at creation, so opening a file that exists changes nothing about it.
pub(super) fn options() -> std::fs::OpenOptions {
    // Setting the mode is the only change made here, and only Unix has one to set: elsewhere the
    // binding is never changed, and the lint that notices would have the two platforms differ in
    // how it is declared.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(FILE_MODE);
    }
    options
}

/// Creates or truncates `path` for writing, as its owner's alone.
pub(super) async fn create(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = options();
    options.write(true).create(true).truncate(true);
    tokio::fs::OpenOptions::from(options).open(path).await
}

/// Takes group and other access away from a directory that grants any.
///
/// Best effort, and never an error: see the module documentation for why a directory that
/// cannot be narrowed is not a reason to refuse the store.
pub(super) fn narrow_dir(dir: &Path) {
    #[cfg(unix)]
    narrow(dir, DIR_MODE);
    #[cfg(not(unix))]
    let _ = dir;
}

/// Takes group and other access away from a file that grants any.
///
/// For a file that was copied rather than created, which carries the mode of what it was
/// copied from.
// The `Result` is the Unix signature; on other platforms the body cannot fail, and the lint
// that notices would have the two platforms disagree on how this is called.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
pub(super) fn narrow_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::symlink_metadata(path)?.permissions().mode();
        if mode & OTHERS != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Sets `dir` to `mode` when it is a real directory that grants something to others.
#[cfg(unix)]
fn narrow(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    // `symlink_metadata`, so a link planted where a directory should be is never followed to
    // change the mode of whatever it points at.
    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !metadata.is_dir() || metadata.permissions().mode() & OTHERS == 0 {
        return;
    }
    if let Err(source) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(
            %source,
            dir = %dir.display(),
            "a store directory other accounts can read could not be made private"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn mode(path: &Path) -> u32 {
        // Zero for a path that is not there, which no assertion below expects.
        std::fs::symlink_metadata(path).map_or(0, |metadata| metadata.permissions().mode() & 0o777)
    }

    #[test]
    fn a_created_directory_and_its_missing_parents_are_the_owners_alone() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let leaf = home.path().join("a/b");
        create_dir_all_blocking(&leaf).unwrap_or_else(|error| panic!("create: {error}"));
        assert_eq!(mode(&leaf), 0o700);
        assert_eq!(mode(&home.path().join("a")), 0o700);
    }

    #[test]
    fn a_directory_others_can_read_is_narrowed_and_a_private_one_is_left() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let open = home.path().join("open");
        std::fs::create_dir(&open).unwrap_or_else(|error| panic!("mkdir: {error}"));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod: {error}"));
        narrow_dir(&open);
        assert_eq!(mode(&open), 0o700);

        // Already private, and stricter than this module would make it: not widened.
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o500))
            .unwrap_or_else(|error| panic!("chmod: {error}"));
        narrow_dir(&open);
        assert_eq!(mode(&open), 0o500);
    }

    #[test]
    fn narrowing_never_follows_a_link_to_change_what_it_points_at() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let outside = home.path().join("outside");
        std::fs::create_dir(&outside).unwrap_or_else(|error| panic!("mkdir: {error}"));
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod: {error}"));
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap_or_else(|error| panic!("{error}"));
        narrow_dir(&link);
        assert_eq!(mode(&outside), 0o755);
    }

    #[test]
    fn a_copied_file_is_narrowed_and_a_created_one_starts_private() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let copied = home.path().join("copied");
        std::fs::write(&copied, b"x").unwrap_or_else(|error| panic!("write: {error}"));
        std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("chmod: {error}"));
        narrow_file(&copied).unwrap_or_else(|error| panic!("narrow: {error}"));
        assert_eq!(mode(&copied), 0o600);

        let created = home.path().join("created");
        options()
            .write(true)
            .create_new(true)
            .open(&created)
            .unwrap_or_else(|error| panic!("open: {error}"));
        assert_eq!(mode(&created), 0o600);
    }
}
