//! Integration tests for the JSONL session store, through
//! [`nanus_ports::StorePort`].
//!
//! An integration-test crate is entirely test code, where a panic *is* the
//! assertion, so the workspace's panic-family exemption is restated here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nanus_adapter_store::{JsonlStore, new_session_id};
use nanus_domain::{Session, SessionEvent, SessionId, TurnEndReason};
use nanus_ports::{StoreError, StorePort};

/// Builds a session with one completed turn containing `messages`.
fn session(id: &str, created_at_ms: u64, messages: &[&str]) -> Session {
    let mut session = Session::new(SessionId::new(id), created_at_ms, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    for text in messages {
        session.append(SessionEvent::UserMessage {
            text: (*text).to_owned(),
        });
    }
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Completed,
    });
    session
}

/// Opens a store over a fresh temporary home.
async fn store() -> (tempfile::TempDir, JsonlStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::new(dir.path()).await.expect("store");
    (dir, store)
}

// ---------------------------------------------------------------------------
// Round trips.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_then_load_round_trips() {
    let (_dir, store) = store().await;
    let original = session("session-1", 1_700_000_000_000, &["hello", "again"]);
    store.save(&original).await.expect("save");
    let loaded = store.load(original.id()).await.expect("load");
    assert_eq!(loaded, original);
    assert_eq!(loaded.event_count(), 4);
}

#[tokio::test]
async fn loading_a_missing_session_is_not_found() {
    let (_dir, store) = store().await;
    let error = store
        .load(&SessionId::new("nope"))
        .await
        .expect_err("must fail");
    match error {
        StoreError::NotFound { id } => assert_eq!(id, "nope"),
        other => panic!("expected NotFound, got {other}"),
    }
}

#[tokio::test]
async fn save_replaces_the_previous_log_wholly() {
    let (_dir, store) = store().await;
    let first = session("same-id", 1, &["one", "two", "three"]);
    store.save(&first).await.expect("save first");
    let second = session("same-id", 2, &["only"]);
    store.save(&second).await.expect("save second");
    let loaded = store.load(second.id()).await.expect("load");
    assert_eq!(loaded, second, "the new log replaced the old one");
    assert_eq!(
        loaded.event_count(),
        3,
        "no events survived from the old log"
    );
}

#[tokio::test]
async fn the_home_is_created_and_returned() {
    let (_dir, store) = store().await;
    let home = store.home().await.expect("home");
    assert!(home.exists(), "the home is created on demand");
    assert_eq!(home, store.home_path());
}

// ---------------------------------------------------------------------------
// Atomic writes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_save_leaves_no_temporary_file_behind() {
    let (_dir, store) = store().await;
    let written = session("atomic-1", 5, &["x"]);
    store.save(&written).await.expect("save");
    let dir = store.session_dir(written.id()).expect("dir");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["session.jsonl".to_owned()], "one file, no temp");
}

#[tokio::test]
async fn a_stale_temporary_file_does_not_affect_a_load() {
    let (_dir, store) = store().await;
    let written = session("atomic-2", 5, &["x"]);
    store.save(&written).await.expect("save");
    let dir = store.session_dir(written.id()).expect("dir");
    // Simulate a crash between create and rename: a leftover temp file.
    std::fs::write(dir.join(".session.jsonl.999.0.tmp"), b"{ truncated").expect("plant");
    let loaded = store
        .load(written.id())
        .await
        .expect("load ignores the temp");
    assert_eq!(loaded, written);
}

// ---------------------------------------------------------------------------
// Damaged files.
// ---------------------------------------------------------------------------

/// Writes `body` over the log of `id`, bypassing the store.
fn overwrite_log(store: &JsonlStore, id: &SessionId, body: &str) {
    let path = store.session_file(id).expect("path");
    std::fs::write(&path, body).expect("overwrite");
}

/// Asserts a load failed with a `Corrupt` error whose message says `needle`.
fn assert_corrupt(error: StoreError, needle: &str) {
    match error {
        StoreError::Corrupt { message, .. } => {
            assert!(
                message.contains(needle),
                "the message names the damage ({needle}): {message}"
            );
        }
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[tokio::test]
async fn a_truncated_tail_is_reported_as_corrupt() {
    let (_dir, store) = store().await;
    let written = session("trunc-1", 7, &["a message"]);
    store.save(&written).await.expect("save");
    let full = written.to_jsonl();
    let mut lines: Vec<&str> = full.lines().collect();
    assert!(lines.len() >= 2, "there is a body to truncate");
    let last = lines.pop().expect("last line");
    // `saturating_div` rather than `/`: the workspace denies integer division.
    let kept = last
        .get(..last.len().saturating_div(2))
        .expect("half a line");
    overwrite_log(
        &store,
        written.id(),
        &format!("{}\n{kept}", lines.join("\n")),
    );
    let error = store.load(written.id()).await.expect_err("must fail");
    assert_corrupt(error, "truncated");
}

#[tokio::test]
async fn a_newer_header_version_is_refused_outright() {
    let (_dir, store) = store().await;
    let written = session("future-1", 9, &["a"]);
    store.save(&written).await.expect("save");
    let body = written
        .to_jsonl()
        .replacen("\"version\":1", "\"version\":99", 1);
    assert!(body.contains("\"version\":99"), "the header was rewritten");
    overwrite_log(&store, written.id(), &body);
    let error = store.load(written.id()).await.expect_err("must fail");
    assert_corrupt(error, "version 99");
}

#[tokio::test]
async fn a_hole_in_the_sequence_is_rejected() {
    let (_dir, store) = store().await;
    let written = session("hole-1", 11, &["a", "b"]);
    store.save(&written).await.expect("save");
    let body = written.to_jsonl().replacen("\"seq\":2", "\"seq\":5", 1);
    assert!(body.contains("\"seq\":5"), "the sequence was rewritten");
    overwrite_log(&store, written.id(), &body);
    let error = store.load(written.id()).await.expect_err("must fail");
    assert_corrupt(error, "sequence 5");
}

#[tokio::test]
async fn a_malformed_body_line_is_rejected() {
    let (_dir, store) = store().await;
    let written = session("bad-1", 13, &["a", "b", "c"]);
    store.save(&written).await.expect("save");
    let mut lines: Vec<String> = written.to_jsonl().lines().map(str::to_owned).collect();
    lines.insert(2, String::from("{not json"));
    overwrite_log(&store, written.id(), &lines.join("\n"));
    let error = store.load(written.id()).await.expect_err("must fail");
    assert_corrupt(error, "malformed");
}

#[tokio::test]
async fn a_bad_header_is_rejected() {
    let (_dir, store) = store().await;
    let id = SessionId::new("badheader-1");
    std::fs::create_dir_all(store.session_dir(&id).expect("dir")).expect("mkdir");
    // A complete header whose format tag is wrong, so the format check is what
    // rejects it rather than a missing field.
    let body = concat!(
        "{\"format\":\"not-nanus\",\"version\":1,",
        "\"id\":\"badheader-1\",\"created_at_ms\":1,\"cwd\":\"/w\"}\n"
    );
    overwrite_log(&store, &id, body);
    let error = store.load(&id).await.expect_err("must fail");
    assert_corrupt(error, "format");
}

// ---------------------------------------------------------------------------
// Listing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_survives_a_damaged_body() {
    let (_dir, store) = store().await;
    let written = session("listed-1", 1_700_000_000_500, &["first", "second"]);
    store.save(&written).await.expect("save");
    // Deliberately destroy the body while keeping the header intact.
    let header = written
        .to_jsonl()
        .lines()
        .next()
        .expect("header")
        .to_owned();
    overwrite_log(
        &store,
        written.id(),
        &format!("{header}\n{{ garbage\n{{ more garbage\n"),
    );
    // A load must fail, but a listing must not: identity comes from the header.
    assert!(
        store.load(written.id()).await.is_err(),
        "the body is damaged"
    );
    let summaries = store.list().await.expect("list");
    assert_eq!(summaries.len(), 1);
    let summary = summaries.first().expect("summary");
    assert_eq!(summary.id, *written.id());
    assert_eq!(summary.created_at_ms, 1_700_000_000_500);
    assert_eq!(summary.cwd, "/work");
    assert!(
        summary.title.is_none(),
        "garbage holds no human turn to title"
    );
}

#[tokio::test]
async fn a_summary_carries_the_title_and_event_count() {
    let (_dir, store) = store().await;
    let written = session("titled-1", 42, &["explain the parser"]);
    store.save(&written).await.expect("save");
    let summaries = store.list().await.expect("list");
    let summary = summaries.first().expect("summary");
    assert_eq!(
        summary.title.as_deref(),
        Some("explain the parser"),
        "the title comes from the first human turn"
    );
    assert_eq!(
        summary.event_count, 3,
        "one turn start, one message, one end"
    );
    assert!(summary.last_event_at_ms > 0, "the file's mtime is reported");
}

#[tokio::test]
async fn list_is_sorted_newest_first() {
    let (_dir, store) = store().await;
    store
        .save(&session("oldest", 1_000, &["a"]))
        .await
        .expect("save");
    store
        .save(&session("newest", 3_000, &["b"]))
        .await
        .expect("save");
    store
        .save(&session("middle", 2_000, &["c"]))
        .await
        .expect("save");
    let summaries = store.list().await.expect("list");
    let ids: Vec<&str> = summaries.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["newest", "middle", "oldest"]);
}

#[tokio::test]
async fn list_skips_a_session_with_an_unreadable_header() {
    let (_dir, store) = store().await;
    store
        .save(&session("good", 10, &["a"]))
        .await
        .expect("save");
    let broken = SessionId::new("broken");
    std::fs::create_dir_all(store.session_dir(&broken).expect("dir")).expect("mkdir");
    overwrite_log(&store, &broken, "not json at all\n");
    let summaries = store.list().await.expect("list");
    assert_eq!(
        summaries.len(),
        1,
        "one bad header must not hide a good one"
    );
    assert_eq!(summaries.first().expect("summary").id.as_str(), "good");
}

#[tokio::test]
async fn list_on_an_empty_store_is_empty() {
    let (_dir, store) = store().await;
    assert!(store.list().await.expect("list").is_empty());
}

// ---------------------------------------------------------------------------
// Deletion.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_removes_the_session_directory() {
    let (_dir, store) = store().await;
    let written = session("doomed", 1, &["bye"]);
    store.save(&written).await.expect("save");
    let dir = store.session_dir(written.id()).expect("dir");
    assert!(dir.exists());
    store.delete(written.id()).await.expect("delete");
    assert!(!dir.exists(), "the directory is gone");
    assert!(store.list().await.expect("list").is_empty());
    // The port is explicit: deleting something already gone is not an error.
    store
        .delete(written.id())
        .await
        .expect("deleting an absent session succeeds");
}

#[tokio::test]
async fn delete_does_not_follow_a_symlink_out_of_the_home() {
    let (_dir, store) = store().await;
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("precious.txt"), "keep me").expect("seed");
    let id = SessionId::new("linked");
    let dir = store.session_dir(&id).expect("dir");
    std::os::unix::fs::symlink(outside.path(), &dir).expect("symlink");
    store.delete(&id).await.expect("delete");
    assert!(
        std::fs::symlink_metadata(&dir).is_err(),
        "the link was removed"
    );
    assert!(
        outside.path().join("precious.txt").exists(),
        "deletion must not follow the link"
    );
}

// ---------------------------------------------------------------------------
// Session ids.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generated_ids_are_time_ordered() {
    let first = new_session_id();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = new_session_id();
    assert_ne!(first, second, "ids are unique");
    assert!(
        second.as_str() > first.as_str(),
        "uuid v7 sorts by creation: {first} then {second}"
    );
}

#[tokio::test]
async fn a_traversing_id_is_encoded_into_one_safe_component() {
    let (_dir, store) = store().await;
    let sneaky = SessionId::new("../../escape");
    let dir = store.session_dir(&sneaky).expect("dir");
    let parent = dir.parent().expect("parent");
    assert_eq!(
        parent.file_name().and_then(|name| name.to_str()),
        Some("sessions"),
        "the id added exactly one component: {}",
        dir.display()
    );
    let written = Session::new(sneaky.clone(), 42, "/work");
    store.save(&written).await.expect("save");
    assert_eq!(store.load(&sneaky).await.expect("load"), written);
}

#[tokio::test]
async fn an_empty_or_relative_id_is_rejected() {
    let (_dir, store) = store().await;
    for raw in ["", ".", ".."] {
        assert!(
            store.session_dir(&SessionId::new(raw)).is_err(),
            "{raw:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn resolve_home_prefers_its_explicit_argument() {
    let dir = tempfile::tempdir().expect("tempdir");
    let resolved = nanus_adapter_store::resolve_home(Some(dir.path())).expect("resolve");
    assert_eq!(resolved, dir.path());
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

    /// An arbitrary session survives a save and a load unchanged.
    ///
    /// The id strategy produces only *usable* ids. The store deliberately rejects `.`,
    /// `..`, and anything that resolves to a relative path component, because a
    /// session id is used as a directory name; generating those here would test the
    /// generator rather than the round trip.
    #[test]
    fn save_then_load_round_trips_any_session(
        id in "[A-Za-z0-9_][A-Za-z0-9._-]{0,23}",
        created_at_ms in 0u64..u64::from(u32::MAX),
        texts in proptest::collection::vec(".{0,64}", 0..6),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = JsonlStore::new(dir.path()).await.expect("store");
            let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
            let original = session(&id, created_at_ms, &borrowed);
            store.save(&original).await.expect("save");
            let loaded = store.load(original.id()).await.expect("load");
            proptest::prop_assert_eq!(loaded, original);
            Ok(())
        }).expect("round trip");
    }
}
