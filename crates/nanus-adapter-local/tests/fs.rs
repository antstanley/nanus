//! Integration tests for the rooted local filesystem adapter, through
//! [`nanus_ports::FsPort`].
//!
//! An integration-test crate is entirely test code, where a panic *is* the
//! assertion. The workspace `clippy.toml` states that intent, but clippy's own
//! test detection does not reach helper functions in an integration-test crate,
//! so the exemption is restated here rather than hidden behind per-call allows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use nanus_adapter_local::LocalFs;
use nanus_ports::{FsError, FsPort, SearchKind, SearchQuery, WriteMode};

/// Creates a rooted adapter over a fresh temporary workspace.
fn workspace() -> (tempfile::TempDir, LocalFs) {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs = LocalFs::new(dir.path()).expect("root");
    (dir, fs)
}

// ---------------------------------------------------------------------------
// Path escape: three directions, one typed error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dot_dot_traversal_is_rejected() {
    let (dir, fs) = workspace();
    let outside = dir
        .path()
        .parent()
        .expect("parent")
        .join("nanus-outside.txt");
    std::fs::write(&outside, "secret").expect("seed");
    let error = fs
        .read(Path::new("../nanus-outside.txt"))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::OutsideWorkspace { .. }), "{error}");
    std::fs::remove_file(&outside).unwrap_or_default();
}

#[tokio::test]
async fn a_deep_dot_dot_traversal_is_rejected() {
    let (_dir, fs) = workspace();
    let error = fs
        .read(Path::new("a/b/../../../etc/hosts"))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::OutsideWorkspace { .. }), "{error}");
}

#[tokio::test]
async fn an_absolute_path_outside_the_root_is_rejected() {
    let (_dir, fs) = workspace();
    let error = fs
        .read(Path::new("/etc/hosts"))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::OutsideWorkspace { .. }), "{error}");
}

#[tokio::test]
async fn a_symlinked_directory_that_escapes_is_rejected() {
    let (dir, fs) = workspace();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.txt"), "secret").expect("seed");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
    // The lexical check passes (the path is inside), so this is the canonicalizing
    // check doing the work.
    let error = fs
        .read(Path::new("link/secret.txt"))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::OutsideWorkspace { .. }), "{error}");
    assert!(
        outside.path().join("secret.txt").exists(),
        "a rejected read does not disturb the target"
    );
}

#[tokio::test]
async fn an_escape_is_never_reported_as_existing() {
    let (_dir, fs) = workspace();
    assert!(
        !fs.exists(Path::new("../nope")).await,
        "exists is infallible by port contract, and an escape does not exist"
    );
    assert!(!fs.exists(Path::new("/etc/hosts")).await);
}

#[tokio::test]
async fn a_path_inside_the_root_resolves() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("ok.txt"), "fine").expect("seed");
    let resolved = fs
        .canonicalize(Path::new("./ok.txt"))
        .await
        .expect("resolve");
    assert!(resolved.starts_with(fs.root()));
    assert_eq!(
        fs.read(Path::new("ok.txt")).await.expect("read").text,
        "fine"
    );
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reads_report_text_and_line_count() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("seed");
    let content = fs.read(Path::new("a.txt")).await.expect("read");
    assert_eq!(content.text, "one\ntwo\nthree\n");
    assert_eq!(content.total_lines, 3);
}

#[tokio::test]
async fn reading_a_directory_is_a_typed_error() {
    let (dir, fs) = workspace();
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
    let error = fs
        .read(Path::new("sub"))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::IsADirectory { .. }), "{error}");
}

#[tokio::test]
async fn reading_non_utf8_is_an_error_not_a_lossy_read() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("raw.bin"), [0xff_u8, 0xfe, 0x00, 0x01]).expect("seed");
    let error = fs
        .read(Path::new("raw.bin"))
        .await
        .expect_err("must be rejected");
    // The port has no dedicated non-UTF-8 variant, so the failure is `Io` and the
    // message says which decoder rejected it.
    assert!(format!("{error}").contains("UTF-8"), "{error}");
}

#[tokio::test]
async fn metadata_and_listing_describe_the_tree() {
    let (dir, fs) = workspace();
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
    std::fs::write(dir.path().join("sub/x.txt"), "hello").expect("seed");
    let metadata = fs.metadata(Path::new("sub/x.txt")).await.expect("metadata");
    assert!(metadata.is_file);
    assert!(!metadata.is_dir);
    assert_eq!(metadata.byte_len, 5);
    let entries = fs.list(Path::new("sub")).await.expect("list");
    assert_eq!(entries.len(), 1);
    let entry = entries.first().expect("entry");
    assert_eq!(entry.name, "x.txt");
    assert!(!entry.is_dir);
    assert!(!entry.is_symlink);
}

// ---------------------------------------------------------------------------
// Writes and edits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_refuses_an_existing_file_and_overwrite_replaces_it() {
    let (_dir, fs) = workspace();
    let first = fs
        .write(Path::new("new.txt"), "first", WriteMode::Create)
        .await
        .expect("create");
    assert!(first.created);
    assert_eq!(first.bytes_written, 5);
    let error = fs
        .write(Path::new("new.txt"), "second", WriteMode::Create)
        .await
        .expect_err("must fail");
    assert!(matches!(error, FsError::AlreadyExists { .. }), "{error}");
    assert_eq!(
        fs.read(Path::new("new.txt")).await.expect("read").text,
        "first"
    );
    let second = fs
        .write(Path::new("new.txt"), "second", WriteMode::Overwrite)
        .await
        .expect("overwrite");
    assert!(!second.created);
    assert_eq!(
        fs.read(Path::new("new.txt")).await.expect("read").text,
        "second"
    );
}

#[tokio::test]
async fn parent_creation_is_opt_in() {
    let (dir, fs) = workspace();
    let nested = Path::new("a/b/c.txt");
    assert!(
        fs.write(nested, "x", WriteMode::Overwrite).await.is_err(),
        "parents are not created unless the adapter is asked to"
    );
    let permissive = LocalFs::new(dir.path())
        .expect("root")
        .with_parent_creation(true);
    permissive
        .write(nested, "x", WriteMode::Overwrite)
        .await
        .expect("explicit creation");
    assert_eq!(permissive.read(nested).await.expect("read").text, "x");
}

#[tokio::test]
async fn edit_returns_the_diff_and_requires_a_unique_match() {
    let (_dir, fs) = workspace();
    fs.write(Path::new("e.txt"), "alpha beta gamma", WriteMode::Overwrite)
        .await
        .expect("seed");
    let result = fs
        .edit(Path::new("e.txt"), "beta", "BETA", false)
        .await
        .expect("edit");
    assert_eq!(result.before, "alpha beta gamma");
    assert_eq!(result.after, "alpha BETA gamma");
    assert_eq!(result.replacements, 1);
    let diff = result.unified_diff();
    assert!(diff.contains("BETA"), "the diff names the change: {diff}");
}

#[tokio::test]
async fn edit_distinguishes_zero_from_many_occurrences() {
    let (_dir, fs) = workspace();
    fs.write(Path::new("e.txt"), "x x x", WriteMode::Overwrite)
        .await
        .expect("seed");
    let missing = fs
        .edit(Path::new("e.txt"), "nope", "y", false)
        .await
        .expect_err("zero occurrences");
    assert!(matches!(missing, FsError::EditNoMatch { .. }), "{missing}");
    let ambiguous = fs
        .edit(Path::new("e.txt"), "x", "y", false)
        .await
        .expect_err("many occurrences");
    assert!(
        matches!(ambiguous, FsError::EditMultipleMatches { count: 3, .. }),
        "{ambiguous}"
    );
    let all = fs
        .edit(Path::new("e.txt"), "x", "y", true)
        .await
        .expect("replace all");
    assert_eq!(all.after, "y y y");
    assert_eq!(all.replacements, 3);
}

// ---------------------------------------------------------------------------
// Search.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_search_matches_paths() {
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}").expect("seed");
    std::fs::write(dir.path().join("README.md"), "# hi").expect("seed");
    let query = SearchQuery::glob(".", "**/*.rs");
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(outcome.matches.len(), 1);
    assert!(
        outcome
            .matches
            .first()
            .expect("match")
            .path
            .ends_with("src/lib.rs")
    );
    assert!(!outcome.truncated);
}

#[tokio::test]
async fn glob_search_reports_truncation_at_the_cap() {
    let (dir, fs) = workspace();
    for index in 0..12 {
        std::fs::write(dir.path().join(format!("f{index}.txt")), "x").expect("seed");
    }
    let query = SearchQuery::glob(".", "*.txt").with_max_results(5);
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(outcome.matches.len(), 5);
    assert!(outcome.truncated, "hitting the cap is reported");
}

#[tokio::test]
async fn literal_search_finds_lines() {
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/a.txt"),
        "let needle = 1;\nlet other = 2;\n",
    )
    .expect("seed");
    let query = SearchQuery::literal(".", "needle");
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(outcome.matches.len(), 1);
    let first = outcome.matches.first().expect("match");
    assert_eq!(first.line_number, 1);
    assert!(first.line.contains("needle"));
    assert_eq!(outcome.files_scanned, 1);
}

#[tokio::test]
async fn binary_files_are_skipped_without_error() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("bin.dat"), b"needle\0needle").expect("seed");
    std::fs::write(dir.path().join("ok.txt"), "needle\n").expect("seed");
    let outcome = fs
        .search(&SearchQuery::literal(".", "needle"))
        .await
        .expect("search");
    assert_eq!(outcome.matches.len(), 1, "binary is skipped, not an error");
    assert!(
        outcome
            .matches
            .first()
            .expect("match")
            .path
            .ends_with("ok.txt")
    );
}

#[tokio::test]
async fn vcs_metadata_is_never_searched() {
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
    std::fs::write(dir.path().join(".git/config"), "needle\n").expect("seed");
    std::fs::write(dir.path().join("visible.txt"), "needle\n").expect("seed");
    let outcome = fs
        .search(&SearchQuery::literal(".", "needle").with_hidden())
        .await
        .expect("search");
    assert_eq!(outcome.matches.len(), 1);
    assert!(
        outcome
            .matches
            .first()
            .expect("match")
            .path
            .ends_with("visible.txt")
    );
}

#[tokio::test]
async fn literal_search_is_case_insensitive_when_asked() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("a.txt"), "Needle\n").expect("seed");
    let sensitive = fs
        .search(&SearchQuery::literal(".", "needle"))
        .await
        .expect("search");
    assert!(sensitive.matches.is_empty());
    let insensitive = fs
        .search(&SearchQuery::literal(".", "needle").with_case_insensitive())
        .await
        .expect("search");
    assert_eq!(insensitive.matches.len(), 1);
}

#[tokio::test]
async fn literal_search_skips_files_over_the_size_cap() {
    let (dir, fs) = workspace();
    std::fs::write(dir.path().join("big.txt"), "needle\n".repeat(100)).expect("seed");
    let query = SearchQuery::literal(".", "needle").with_max_file_bytes(10);
    let outcome = fs.search(&query).await.expect("search");
    assert!(outcome.matches.is_empty(), "the file was over the cap");
    assert_eq!(outcome.files_scanned, 0);
}

#[tokio::test]
async fn a_bad_glob_is_a_typed_error() {
    let (_dir, fs) = workspace();
    let error = fs
        .search(&SearchQuery::glob(".", "["))
        .await
        .expect_err("must be rejected");
    assert!(matches!(error, FsError::InvalidPattern { .. }), "{error}");
}

#[tokio::test]
async fn search_kinds_are_distinguishable() {
    let query = SearchQuery::literal(".", "x");
    assert_eq!(query.kind, SearchKind::Literal);
    assert!(query.case_sensitive);
    assert!(!query.include_hidden);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// `read` then `write` then `read` round-trips arbitrary UTF-8 content.
    #[test]
    fn write_then_read_round_trips(text in ".{0,4000}") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, fs) = workspace();
            fs.write(Path::new("round.txt"), &text, WriteMode::Overwrite)
                .await
                .expect("write");
            let first = fs.read(Path::new("round.txt")).await.expect("read");
            proptest::prop_assert_eq!(&first.text, &text);
            fs.write(Path::new("round.txt"), &first.text, WriteMode::Overwrite)
                .await
                .expect("rewrite");
            let second = fs.read(Path::new("round.txt")).await.expect("reread");
            proptest::prop_assert_eq!(first, second);
            Ok(())
        }).expect("round trip");
    }

    /// An edit that replaces a unique anchor always changes exactly that anchor.
    #[test]
    fn edits_are_local_to_their_anchor(
        prefix in "[a-z]{0,20}",
        anchor in "[A-Z]{1,8}",
        suffix in "[a-z]{0,20}",
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, fs) = workspace();
            let body = format!("{prefix}{anchor}{suffix}");
            fs.write(Path::new("e.txt"), &body, WriteMode::Overwrite)
                .await
                .expect("seed");
            let result = fs
                .edit(Path::new("e.txt"), &anchor, "REPLACED", false)
                .await
                .expect("edit");
            proptest::prop_assert_eq!(result.after, format!("{prefix}REPLACED{suffix}"));
            Ok(())
        }).expect("edit");
    }
}

#[tokio::test]
async fn a_bare_glob_is_anchored_to_the_search_root() {
    // The bug this pins: under `globset`'s defaults `*` matches a directory separator, and
    // matching against an absolute path made every bare pattern match. So `*.rs` returned
    // files at any depth — a tool named `glob` that silently searched recursively. A model
    // noticed and reported it before a test did.
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
    std::fs::write(dir.path().join("root.rs"), "// top level").expect("seed");
    std::fs::write(dir.path().join("src/lib.rs"), "// one level down").expect("seed");
    std::fs::write(dir.path().join("src/deep/nested.rs"), "// two levels down").expect("seed");

    let query = SearchQuery::glob(".", "*.rs");
    let outcome = fs.search(&query).await.expect("search");
    // Exactly the file directly in the root, and nothing else.
    assert_eq!(
        outcome.matches.len(),
        1,
        "a bare glob matches only the root level: {:?}",
        outcome.matches
    );
    assert!(
        outcome
            .matches
            .first()
            .expect("match")
            .path
            .ends_with("root.rs"),
        "and it is the top-level file"
    );
}

#[tokio::test]
async fn a_double_star_glob_reaches_every_depth() {
    // The positive counterpart: recursion is available, but it has to be asked for.
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
    std::fs::write(dir.path().join("root.rs"), "// top").expect("seed");
    std::fs::write(dir.path().join("src/lib.rs"), "// one").expect("seed");
    std::fs::write(dir.path().join("src/deep/nested.rs"), "// two").expect("seed");

    let query = SearchQuery::glob(".", "**/*.rs");
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(
        outcome.matches.len(),
        3,
        "a recursive glob finds all three: {:?}",
        outcome.matches
    );
}

#[tokio::test]
async fn a_directory_prefixed_glob_is_anchored_too() {
    let (dir, fs) = workspace();
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "// one").expect("seed");
    std::fs::write(dir.path().join("src/deep/nested.rs"), "// two").expect("seed");

    // `src/*.rs` means one level inside `src`, not everything beneath it.
    let query = SearchQuery::glob(".", "src/*.rs");
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(
        outcome.matches.len(),
        1,
        "a directory-prefixed glob stops at the separator: {:?}",
        outcome.matches
    );
    assert!(
        outcome
            .matches
            .first()
            .expect("match")
            .path
            .ends_with("lib.rs")
    );

    // And the recursive form reaches both.
    let query = SearchQuery::glob(".", "src/**/*.rs");
    let outcome = fs.search(&query).await.expect("search");
    assert_eq!(outcome.matches.len(), 2, "{:?}", outcome.matches);
}
