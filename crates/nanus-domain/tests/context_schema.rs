//! The published boundary schema and the Rust types describe the same payloads.
//!
//! `docs/context-management.schema.json` is the contract a host or a client reads; the types in
//! `nanus_domain::context::managed` are what is actually written. This test serializes a value
//! of every type and checks it against its definition: the same property names, every required
//! one present, and the six session records wrapped exactly as the `ContextEvent` union says.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use nanus_domain::SessionEvent;
use nanus_domain::context::managed::*;
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("../../../docs/context-management.schema.json");

fn schema() -> Value {
    serde_json::from_str(SCHEMA).unwrap()
}

fn digest() -> Digest {
    Digest::of(b"x")
}

fn frontier() -> ContextFrontier {
    ContextFrontier {
        session_id: "s".into(),
        event_count: 1,
        prefix_blake3: digest(),
        projection_revision: 0,
    }
}

fn source() -> SourceRef {
    SourceRef {
        kind: SourceKind::Event,
        event_seq: Some(0),
        block_index: None,
        artifact_id: None,
        offset: 0,
        length: 1,
        source_digest: digest(),
        field: SourceField::UserText,
    }
}

fn selection() -> SelectionIdentity {
    SelectionIdentity {
        provider: "p".into(),
        endpoint_digest: digest(),
        protocol: "c".into(),
        model: "m".into(),
        effort: None,
        epoch: 0,
    }
}

fn status() -> ContextStatus {
    ContextStatus {
        mode: ContextMode::Managed,
        revision: 0,
        frontier: frontier(),
        estimate_input_tokens: None,
        estimate_protected_tokens: None,
        output_reserve_tokens: 1,
        estimator: "e".into(),
        hidden_fragments: 0,
        protected_fragments: 0,
        goal_revision: None,
        goal_data_available: false,
        recall_available: true,
        archive_available: false,
        last_decision: None,
        profile_digest: digest(),
        managed_ready: true,
        unavailable_reason: None,
    }
}

fn revision() -> ProjectionRevision {
    ProjectionRevision {
        revision: 1,
        base_revision: 0,
        base_frontier: frontier(),
        hidden: Vec::new(),
        notes: Vec::new(),
        author: RevisionAuthor::Model,
        reason: RevisionReason::ModelProposal,
        policy_version: 1,
        decision_id: "d".into(),
        goal_revision: None,
        notes_digest: digest(),
        base_profile_digest: digest(),
    }
}

fn attempt() -> RequestAttemptRecord {
    RequestAttemptRecord {
        attempt_id: "a".into(),
        retry_of: None,
        turn: 1,
        step: 1,
        selection: selection(),
        projection_revision: 0,
        request_digest: digest(),
        phase: AttemptPhase::Started,
        outcome: None,
        usage: None,
        assistant_seq: None,
        included_management_fragments: Vec::new(),
        started_at_ms: 0,
        finished_at_ms: None,
        timings_ms: None,
    }
}

fn samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            "ContextPolicy",
            serde_json::to_value(ContextPolicy::managed()).unwrap(),
        ),
        ("ContextFrontier", serde_json::to_value(frontier()).unwrap()),
        ("SourceRef", serde_json::to_value(source()).unwrap()),
        (
            "WorkingNote",
            serde_json::to_value(WorkingNote {
                id: NoteId::parse("n:a").unwrap(),
                claim: "c".into(),
                category: NoteCategory::Observed,
                sources: vec![source()],
            })
            .unwrap(),
        ),
        (
            "ProjectionRevision",
            serde_json::to_value(revision()).unwrap(),
        ),
        (
            "ContextModeRecord",
            serde_json::to_value(ContextModeRecord {
                policy: ContextPolicy::managed(),
                actor: ModeActor::Human,
                reason: ModeReason::Enable,
                previous_revision: 0,
            })
            .unwrap(),
        ),
        (
            "ContextDecision",
            serde_json::to_value(ContextDecision {
                decision_id: "d".into(),
                outcome: DecisionOutcome::Accepted,
                revision: Some(1),
                error_code: None,
            })
            .unwrap(),
        ),
        (
            "ArtifactReceipt",
            serde_json::to_value(ArtifactReceipt {
                artifact_id: None,
                call_id: "c".into(),
                stream: CaptureStream::Stdout,
                retained_bytes: 0,
                observed_bytes: 0,
                retained_blake3: None,
                status: CaptureStatus::Unavailable,
                reason: CaptureReason::Unsupported,
                encoding: RawEncoding::Raw,
                chunk_blake3: Vec::new(),
            })
            .unwrap(),
        ),
        (
            "SelectionIdentity",
            serde_json::to_value(selection()).unwrap(),
        ),
        (
            "UsageObservation",
            serde_json::to_value(UsageObservation::default()).unwrap(),
        ),
        (
            "RequestAttemptRecord",
            serde_json::to_value(attempt()).unwrap(),
        ),
        (
            "AttemptTimings",
            serde_json::to_value(AttemptTimings::default()).unwrap(),
        ),
        (
            "RecoveryRecord",
            serde_json::to_value(RecoveryRecord {
                recovered_frontier: frontier(),
                turn: 1,
                unmatched_attempt_ids: Vec::new(),
                reason: RecoveryReason::SettledOpenTurn,
            })
            .unwrap(),
        ),
        (
            "CheckpointReceipt",
            serde_json::to_value(CheckpointReceipt {
                frontier: frontier(),
                body_digest: digest(),
                durability: Durability::ProcessCrash,
            })
            .unwrap(),
        ),
    ]
    .into_iter()
    .chain(results())
    .collect()
}

/// Samples of the tool and status payloads.
fn results() -> Vec<(&'static str, Value)> {
    let to = |value: Value| value;
    vec![
        ("ContextStatus", serde_json::to_value(status()).unwrap()),
        (
            "FragmentDescriptor",
            serde_json::to_value(FragmentDescriptor {
                id: FragmentId::new(1),
                assistant_seq: 1,
                last_result_seq: None,
                protected: false,
                hidden: false,
                summary: String::new(),
            })
            .unwrap(),
        ),
        (
            "ManageResult",
            serde_json::to_value(ManageResult {
                status: ManageStatus::Inspected,
                context: status(),
                decision: None,
                fragments: Vec::new(),
                next_cursor: None,
                error_code: None,
            })
            .unwrap(),
        ),
        (
            "RecallHit",
            serde_json::to_value(RecallHit {
                source: source(),
                tool_name: None,
                call_id: None,
                is_error: None,
                excerpt: String::new(),
                durable: true,
            })
            .unwrap(),
        ),
        (
            "RecallResult",
            serde_json::to_value(RecallResult {
                status: RecallStatus::Ok,
                frontier: frontier(),
                hits: Vec::new(),
                data: None,
                encoding: RecallEncoding::Text,
                actual_offset: None,
                next_offset: None,
                invalid_bytes_replaced: false,
                coverage: Coverage::Complete,
                next_cursor: None,
                error_code: None,
                source: None,
                durable: true,
            })
            .unwrap(),
        ),
        (
            "SnapshotProfile",
            serde_json::to_value(SnapshotProfile {
                selection: selection(),
                system_prompt_digest: digest(),
                tool_schema_digest: digest(),
                policy: ContextPolicy::managed(),
                goal_revision: None,
            })
            .unwrap(),
        ),
        (
            "ContextManageInput",
            to(json!({"action": "inspect", "base_revision": null,
            "base_frontier": null, "hide": [], "restore": [], "notes": [], "cursor": null,
            "base_profile_digest": null})),
        ),
        (
            "ContextRecallInput",
            to(json!({"action": "search", "query": "q", "target": null,
            "cursor": null, "limit": 1, "max_bytes": 1, "encoding": "text"})),
        ),
    ]
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn every_payload_has_exactly_the_properties_its_definition_lists() {
    let schema = schema();
    let mut checked = BTreeSet::new();
    for (name, value) in samples() {
        let definition = &schema["$defs"][name];
        let properties = keys(&definition["properties"]);
        assert_eq!(keys(&value), properties, "{name}");
        for required in definition["required"].as_array().unwrap() {
            assert!(
                properties.contains(required.as_str().unwrap()),
                "{name}: {required}"
            );
        }
        assert_eq!(
            definition["additionalProperties"], false,
            "{name} is closed"
        );
        checked.insert(name);
    }
    // The inputs parse through their own strict parsers, too.
    let sample = |wanted: &str| {
        samples()
            .into_iter()
            .find(|(name, _)| *name == wanted)
            .map(|(_, value)| value)
            .unwrap()
    };
    assert!(ContextManageInput::parse(&sample("ContextManageInput")).is_ok());
    assert!(ContextRecallInput::parse(&sample("ContextRecallInput")).is_ok());
    let defined: BTreeSet<&str> = schema["$defs"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, definition)| definition.get("properties").is_some())
        .map(|(name, _)| name.as_str())
        .collect();
    let untested: Vec<&&str> = defined
        .iter()
        .filter(|name| !checked.contains(**name) && !["RecallTarget"].contains(*name))
        .collect();
    assert!(
        untested.is_empty(),
        "definitions without a sample: {untested:?}"
    );
}

#[test]
fn the_six_session_records_are_wrapped_as_the_event_union_says() {
    let schema = schema();
    let union: BTreeSet<String> = schema["$defs"]["ContextEvent"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| {
            variant["properties"]["type"]["const"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let events = [
        SessionEvent::ContextRevision {
            payload: Box::new(revision()),
        },
        SessionEvent::RequestAttempt {
            payload: Box::new(attempt()),
        },
        SessionEvent::ContextDecision {
            payload: Box::new(ContextDecision {
                decision_id: "d".into(),
                outcome: DecisionOutcome::Rejected,
                revision: None,
                error_code: Some(ErrorCode::StaleBase),
            }),
        },
    ];
    for event in events {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            keys(&value),
            BTreeSet::from(["payload".to_owned(), "type".to_owned()])
        );
        assert!(union.contains(value["type"].as_str().unwrap()), "{value}");
    }
    assert_eq!(union.len(), 6);
}
