//! Integration tests for the adapter's ranged read: bounded windows of a file, read through a
//! handle checked against the workspace root, with the identity and digest each window carries.
//!
//! An integration-test crate is entirely test code, where a panic *is* the assertion; see the
//! same exemption in `tests/fs.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use nanus_adapter_local::LocalFs;
use nanus_domain::context::managed::Digest;
use nanus_ports::{FsError, FsPort, RANGE_READ_MAX_BYTES};

/// Creates a rooted adapter over a fresh temporary workspace.
fn workspace() -> (tempfile::TempDir, LocalFs) {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs = LocalFs::new(dir.path()).expect("root");
    (dir, fs)
}

/// Five MiB and a bit of bytes whose value depends on their position, so a window read from
/// the wrong offset cannot pass for the right one.
fn patterned(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| u8::try_from(index % 251).expect("below 251"))
        .collect()
}

const FIVE_MIB: usize = 5 * 1024 * 1024 + 17;

#[tokio::test]
async fn a_file_over_four_mebibytes_is_read_in_windows() {
    let (dir, fs) = workspace();
    let contents = patterned(FIVE_MIB);
    std::fs::write(dir.path().join("big.bin"), &contents).expect("seed");

    let first = fs
        .read_range(Path::new("big.bin"), 0, 4_096)
        .await
        .expect("the first window");
    assert_eq!(first.offset, 0);
    assert_eq!(first.bytes, contents[..4_096]);
    assert_eq!(first.range_blake3, Digest::of(&contents[..4_096]));
    assert_eq!(first.identity.len, u64::try_from(FIVE_MIB).expect("fits"));
    assert!(
        !first.eof,
        "a window at the start of a large file is not the end"
    );

    // A window past the four-MiB line the whole-file read refuses, running to the end.
    let tail_start = FIVE_MIB - 1_000;
    let tail = fs
        .read_range(
            Path::new("big.bin"),
            u64::try_from(tail_start).expect("fits"),
            4_096,
        )
        .await
        .expect("the last window");
    assert_eq!(tail.bytes, contents[tail_start..]);
    assert_eq!(tail.range_blake3, Digest::of(&contents[tail_start..]));
    assert!(tail.eof, "the window reached the end of the file");
    assert_eq!(
        first.identity, tail.identity,
        "nothing changed between the two"
    );
}

#[tokio::test]
async fn a_huge_single_line_is_read_in_bounded_windows() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("line.txt"), "x".repeat(FIVE_MIB)).expect("seed");
    let window = fs
        .read_range(Path::new("line.txt"), 1_000_000, usize::MAX)
        .await
        .expect("a window of one line");
    assert_eq!(
        window.bytes.len(),
        RANGE_READ_MAX_BYTES,
        "bounded by the port"
    );
    assert!(window.bytes.iter().all(|byte| *byte == b'x'));
    assert!(!window.eof);
}

#[tokio::test]
async fn a_window_above_the_port_maximum_is_clamped_and_one_below_it_is_not() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("big.bin"), patterned(FIVE_MIB)).expect("seed");
    let clamped = fs
        .read_range(Path::new("big.bin"), 0, RANGE_READ_MAX_BYTES + 1)
        .await
        .expect("clamped, not refused");
    assert_eq!(clamped.bytes.len(), RANGE_READ_MAX_BYTES);
    assert!(!clamped.eof, "a clamped window says there is more");

    let exact = fs
        .read_range(Path::new("big.bin"), 0, 100)
        .await
        .expect("a small window");
    assert_eq!(
        exact.bytes.len(),
        100,
        "a window below the cap is what was asked"
    );
}

#[tokio::test]
async fn an_offset_past_the_end_is_an_empty_window_at_the_end() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("small.txt"), "0123456789").expect("seed");
    let past = fs
        .read_range(Path::new("small.txt"), 50, 10)
        .await
        .expect("past the end is an answer, not an error");
    assert!(past.bytes.is_empty());
    assert!(past.eof);
    assert_eq!(past.offset, 50);
    assert_eq!(past.range_blake3, Digest::empty());

    // The boundary from the other side: the last byte is still a byte.
    let last = fs
        .read_range(Path::new("small.txt"), 9, 10)
        .await
        .expect("the last byte");
    assert_eq!(last.bytes, b"9");
    assert!(last.eof);
}

#[tokio::test]
async fn an_untouched_file_keeps_its_identity_and_a_rewritten_one_does_not() {
    let (dir, fs) = workspace();
    let path = dir.path().join("log.txt");
    std::fs::write(&path, "first version\n").expect("seed");
    let before = fs
        .read_range(Path::new("log.txt"), 0, 5)
        .await
        .expect("first");
    let again = fs
        .read_range(Path::new("log.txt"), 5, 5)
        .await
        .expect("second");
    assert_eq!(before.identity, again.identity, "nothing touched the file");

    std::fs::write(&path, "a second, longer version\n").expect("rewrite");
    let after = fs
        .read_range(Path::new("log.txt"), 0, 5)
        .await
        .expect("third");
    assert_ne!(
        before.identity, after.identity,
        "a rewrite changes the identity"
    );
    assert_ne!(before.range_blake3, after.range_blake3);
}

/// A same-length replacement by rename is the case a length check alone misses: the file id
/// is what changes.
#[cfg(unix)]
#[tokio::test]
async fn a_same_length_replacement_is_a_new_identity() {
    let (dir, fs) = workspace();
    let path = dir.path().join("config.txt");
    std::fs::write(&path, "alpha\n").expect("seed");
    let before = fs
        .read_range(Path::new("config.txt"), 0, 64)
        .await
        .expect("first");
    let staged = dir.path().join("config.txt.new");
    std::fs::write(&staged, "omega\n").expect("stage");
    std::fs::rename(&staged, &path).expect("replace");
    let after = fs
        .read_range(Path::new("config.txt"), 0, 64)
        .await
        .expect("second");
    assert_eq!(before.identity.len, after.identity.len);
    assert_ne!(before.identity.file_id, after.identity.file_id);
    assert_ne!(before.identity, after.identity);
}

#[cfg(unix)]
#[tokio::test]
async fn a_ranged_read_through_a_link_out_of_the_workspace_is_refused() {
    let (dir, fs) = workspace();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.txt"), "secret").expect("seed");
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("leak.txt"),
    )
    .expect("symlink");
    let refused = fs
        .read_range(Path::new("leak.txt"), 0, 64)
        .await
        .expect_err("a link out of the root must be refused");
    assert!(
        matches!(refused, FsError::OutsideWorkspace { .. }),
        "{refused}"
    );

    // The other direction: a link that stays inside the workspace is an ordinary read.
    std::fs::write(dir.path().join("real.txt"), "inside").expect("seed");
    std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("alias.txt"))
        .expect("symlink");
    let inside = fs
        .read_range(Path::new("alias.txt"), 0, 64)
        .await
        .expect("an in-root link is followed");
    assert_eq!(inside.bytes, b"inside");
}

#[tokio::test]
async fn a_ranged_read_outside_the_root_or_of_a_directory_is_a_typed_error() {
    let (dir, fs) = workspace();
    let escape = fs
        .read_range(Path::new("../anything"), 0, 64)
        .await
        .expect_err("escape");
    assert!(
        matches!(escape, FsError::OutsideWorkspace { .. }),
        "{escape}"
    );
    std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
    let directory = fs
        .read_range(Path::new("sub"), 0, 64)
        .await
        .expect_err("a directory has no bytes to window");
    assert!(
        matches!(directory, FsError::IsADirectory { .. }),
        "{directory}"
    );
    let missing = fs
        .read_range(Path::new("missing.txt"), 0, 64)
        .await
        .expect_err("missing");
    assert!(matches!(missing, FsError::NotFound { .. }), "{missing}");
}
