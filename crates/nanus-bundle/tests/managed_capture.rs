//! Shell capture end to end: the real store, the real shell, the real toolset, a managed turn.
//!
//! A command prints a marker past the preview cap. The model sees a bounded preview naming the
//! archive; the session publishes a receipt; the checkpoint that references the object verifies
//! it on disk; and a later recall finds the marker the preview never showed (T23, T25).
#![cfg(all(unix, feature = "stock-compose"))]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use harness::*;
use nanus_adapter_local::{LocalFs, LocalShell};
use nanus_adapter_store::JsonlStore;
use nanus_bundle::{
    CaptureBroker, SessionContext, Silent, StoreCheckpoint, ToolRegistryHandle, TurnHost,
};
use nanus_domain::SessionEvent;
use nanus_domain::context::managed::{CaptureStatus, ContextPolicy, ModeActor};
use nanus_ports::{PersistenceState, SandboxPolicy, TurnRuntime};

const SCRIPT: &str = "printf '%70000s' '' | tr ' ' x; printf 'MARKER-PAST-THE-CAP'";

#[test]
fn archived_output_past_the_preview_is_published_verified_and_recalled() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    nanus_kernel::runtime::block_on_local(async {
        let store = JsonlStore::new(home.path()).await.unwrap().handle();
        let fs = LocalFs::new(workspace.path()).unwrap().handle();
        let policy = SandboxPolicy::new(
            nanus_domain::SandboxMode::DangerFullAccess,
            workspace.path().to_path_buf(),
        );
        let shell = LocalShell::new(policy).handle();
        let broker = CaptureBroker::new();
        let tools = nanus_bundle::build_toolset_with_capture(&fs, &shell, &broker).unwrap();
        let model = <Model as ModelExt>::new(Vec::new());
        let bash = serde_json::json!({ "command": SCRIPT }).to_string();
        model.push(call("b1", "bash", &bash));
        model.push(call(
            "s1",
            "context_recall",
            r#"{"action": "search", "query": "MARKER-PAST-THE-CAP", "target": null,
                "cursor": null, "limit": 5, "max_bytes": 8192, "encoding": "text"}"#,
        ));
        model.push(text("found it"));
        let runner =
            runner_with(&model, ToolRegistryHandle::new(tools), 64_000).with_capture(broker);

        let mut session = session();
        store.lock(session.id(), "test").await.unwrap();
        let checkpoint = StoreCheckpoint::bind(store.clone(), session.id().clone())
            .await
            .unwrap();
        let context = SessionContext::with_key(Some(store.clone()), [3; 32]);
        let runtime = TurnRuntime {
            context: Some(&context),
            checkpoint: Some(&checkpoint),
        };
        let policy = ContextPolicy {
            capture_shell: true,
            ..ContextPolicy::managed()
        };
        runner
            .set_context_policy(&mut session, policy, ModeActor::Human, runtime)
            .await
            .unwrap();
        let host = TurnHost {
            approver: None,
            control: None,
            runtime,
        };
        let run = runner
            .run_turn_with_runtime(&mut session, "run it", &mut Silent, host)
            .await;
        assert!(run.outcome.is_ok(), "{:?}", run.outcome);
        assert!(matches!(
            run.persistence,
            Some(PersistenceState::Acknowledged(_))
        ));

        let (preview, _) = tool_result(&session, "b1");
        assert!(
            !preview.contains("MARKER-PAST-THE-CAP"),
            "the marker is past the preview"
        );
        assert!(preview.contains("stdout archived as a:"), "{preview}");
        let receipts: Vec<_> = session
            .log()
            .events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ArtifactPublished { payload } => Some(payload.clone()),
                _ => None,
            })
            .collect();
        let stdout = receipts
            .iter()
            .find(|receipt| receipt.stream == nanus_domain::context::managed::CaptureStream::Stdout)
            .unwrap();
        assert_eq!(stdout.status, CaptureStatus::Complete);
        assert_eq!(stdout.retained_bytes, 70_019);

        let found = tool_result_json(&session, "s1");
        let hits = found["hits"].as_array().unwrap();
        assert!(
            hits.iter().any(|hit| hit["source"]["kind"] == "artifact"),
            "the marker is found in the archive: {found}"
        );
        // The store holds exactly the session the runner holds, archive references verified.
        let stored = store.load(session.id()).await.unwrap();
        assert_eq!(stored, session);
    });
}
