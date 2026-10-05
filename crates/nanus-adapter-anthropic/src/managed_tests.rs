//! Managed preparation over a real socket, the argument limits, and signed replay under edits.
//!
//! The request each wire fixture prepares is compiled by the domain's own managed compiler from
//! a session — two leading system messages, the generated working-data message after the first
//! user message, and one hidden fragment absent — so the grammar under test is the one the
//! runner produces. The server captures the body it received, which is what lets the assertions
//! be about what was sent rather than what was encoded.

// Fixtures compute exact byte boundaries (a limit and one byte over it) on small, known sizes.
#![allow(clippy::arithmetic_side_effects, clippy::integer_division)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use futures::StreamExt as _;
use nanus_domain::context::managed::compile::{NoticeFacts, Selection, derive_effective_context};
use nanus_domain::context::managed::{Digest, FragmentId, fragments, state};
use nanus_domain::message::AssistantReplay;
use nanus_domain::{
    Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName, ToolSchema,
};
use nanus_ports::{
    ChatRequest, LlmEvent, LlmPort as _, ManagedRequest, ManagedSupport, OVERSIZED_ARGUMENTS,
    PreparedModelCall as _, ReasoningEffort,
};
use serde_json::{Value, json};

use super::managed::{PROTOCOL, PreparedMessages, prepare};
use crate::{AnthropicConfig, AnthropicLlm};

const MODEL: &str = "claude-opus-5-5";
const SIGNATURE: &str = "EqQBCkYIBxgCKkDsignedbytesthatnobodymayrewrite";

fn work(session: &mut Session, id: &str, output: &str) {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![call(id)],
        usage: None,
        interrupted: false,
        model: Some(MODEL.into()),
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new(id),
        content: output.to_owned(),
        content_blocks: None,
        is_error: false,
    });
}

fn call(id: &str) -> ToolCall {
    ToolCall::new(
        ToolCallId::new(id),
        ToolName::new("read").unwrap(),
        json!({ "path": id }),
    )
}

/// A managed session with an early fragment that can be hidden and two recent protected ones.
fn session() -> Session {
    let mut session = Session::new(SessionId::new("managed-wire"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: "constraint: never touch main.rs".into(),
    });
    work(&mut session, "a", "OBSOLETE EARLY OUTPUT");
    work(&mut session, "b", "kept output b");
    session.append(SessionEvent::UserMessage {
        text: "now the second part".into(),
    });
    work(&mut session, "c", "kept output c");
    work(&mut session, "d", "kept output d");
    session
}

/// Compiles the managed request the runner would send, hiding the oldest eligible fragment.
fn compiled(session: &Session) -> ChatRequest {
    let derived = fragments::derive(session.log()).unwrap();
    let protected = state::protected(session.log(), &derived);
    let hidden: Vec<FragmentId> = derived
        .all()
        .iter()
        .map(|fragment| fragment.id)
        .filter(|id| !protected.contains(id))
        .take(1)
        .collect();
    assert_eq!(hidden.len(), 1, "one fragment is hideable");
    let selection = Selection {
        revision: 1,
        hidden: &hidden,
        notes: &[],
        notes_goal_revision: None,
    };
    let facts = NoticeFacts {
        input_allowance: 100_000,
        ..NoticeFacts::default()
    };
    let effective =
        derive_effective_context(session, &derived, &protected, selection, None, &facts).unwrap();
    assert!(effective.memory, "the catalog makes a generated message");
    let mut messages = vec![Message::system("you are nanus"), effective.notice];
    messages.extend(effective.messages);
    request(messages)
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest::new(MODEL, messages)
        .with_tools(vec![schema("read"), schema("context_manage")])
        .with_max_tokens(4_096)
}

fn schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: ToolName::new(name).unwrap(),
        description: format!("the {name} tool"),
        parameters: json!({ "type": "object" }),
    }
}

fn llm() -> AnthropicLlm {
    AnthropicLlm::new(AnthropicConfig::new(MODEL, "test-key")).unwrap()
}

fn managed(request: ChatRequest) -> ManagedRequest {
    ManagedRequest {
        request,
        selection_epoch: 7,
    }
}

/// Reads one HTTP request and returns its body bytes.
fn read_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = stream.read(&mut chunk).unwrap_or(0);
        if count == 0 {
            return Vec::new();
        }
        buffer.extend_from_slice(&chunk[..count]);
        let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..split]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().unwrap());
        let body = split + 4;
        if buffer.len() >= body + length {
            return buffer[body..body + length].to_vec();
        }
    }
}

/// Serves one scripted SSE response on `listener` and returns the body it received.
fn serve_once(listener: TcpListener, sse: String) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let body = read_body(&mut stream);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            sse.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(sse.as_bytes()).unwrap();
        body
    })
}

/// One event-typed SSE frame.
fn event(payload: &Value) -> String {
    format!(
        "event: {}\ndata: {payload}\n\n",
        payload["type"].as_str().unwrap()
    )
}

/// A response whose blocks are `blocks` (each `(start, deltas)`), ending at `message_stop`.
fn messages_stream(blocks: &[(Value, Vec<Value>)]) -> String {
    let mut sse = event(&json!({ "type": "message_start",
        "message": { "usage": { "input_tokens": 10, "output_tokens": 1 } } }));
    for (index, (start, deltas)) in blocks.iter().enumerate() {
        sse.push_str(&event(
            &json!({ "type": "content_block_start", "index": index,
            "content_block": start }),
        ));
        for delta in deltas {
            sse.push_str(&event(
                &json!({ "type": "content_block_delta", "index": index,
                "delta": delta }),
            ));
        }
        sse.push_str(&event(
            &json!({ "type": "content_block_stop", "index": index }),
        ));
    }
    sse.push_str(&event(&json!({ "type": "message_delta",
        "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } })));
    sse.push_str(&event(&json!({ "type": "message_stop" })));
    sse
}

/// A tool-use block for `name` whose arguments stream as `raw` in two fragments.
fn tool_block(name: &str, raw: &str) -> (Value, Vec<Value>) {
    let (first, second) = raw.split_at(raw.len() / 2);
    (
        json!({ "type": "tool_use", "id": "toolu_1", "name": name, "input": {} }),
        vec![
            json!({ "type": "input_json_delta", "partial_json": "" }),
            json!({ "type": "input_json_delta", "partial_json": first }),
            json!({ "type": "input_json_delta", "partial_json": second }),
        ],
    )
}

fn thinking_block() -> (Value, Vec<Value>) {
    (
        json!({ "type": "thinking", "thinking": "" }),
        vec![
            json!({ "type": "thinking_delta", "thinking": "considering" }),
            json!({ "type": "signature_delta", "signature": SIGNATURE }),
        ],
    )
}

/// Sends a prepared call to a fixture server, proving nothing connects before the first poll.
async fn exchange(mut prepared: PreparedMessages, sse: String) -> (Vec<u8>, Vec<LlmEvent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    prepared.retarget(format!("http://{address}/v1/messages"));
    listener.set_nonblocking(true).unwrap();
    let stream = Box::new(prepared).stream();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no connection is made before the stream is polled"
    );
    listener.set_nonblocking(false).unwrap();
    let server = serve_once(listener, sse);
    let events: Vec<LlmEvent> = stream.collect().await;
    (server.join().unwrap(), events)
}

/// Every argument fragment the stream reported, joined as the loop joins them.
fn arguments(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } => Some(arguments_delta.as_str()),
            _ => None,
        })
        .collect()
}

fn replay_of(events: &[LlmEvent]) -> Option<&AssistantReplay> {
    events.iter().find_map(|event| match event {
        LlmEvent::AssistantReplay(replay) => Some(replay),
        _ => None,
    })
}

/// Raw `context_manage` arguments exactly `bytes` long.
fn manage_arguments(bytes: usize) -> String {
    let frame = r#"{"action":"inspect","pad":""}"#;
    let raw = format!(
        r#"{{"action":"inspect","pad":"{}"}}"#,
        "x".repeat(bytes - frame.len())
    );
    assert_eq!(raw.len(), bytes);
    raw
}

#[tokio::test]
async fn a_managed_request_sends_exactly_the_bytes_it_prepared() {
    let llm = llm();
    let request = compiled(&session());
    let prepared = prepare(&llm, managed(request.clone())).unwrap();
    let body = prepared.body().to_owned();
    let digest = prepared.request_digest().clone();
    let estimate = prepared.estimate();
    let (received, events) = exchange(prepared, messages_stream(&[])).await;

    assert_eq!(
        received,
        body.as_bytes(),
        "the server got the prepared body"
    );
    assert_eq!(Digest::of(&received), digest);
    let sent: Value = serde_json::from_slice(&received).unwrap();
    let caps = llm.capabilities(MODEL);
    let again = nanus_ports::capabilities::estimate_payload(caps, &request, &sent).unwrap();
    assert_eq!(again, estimate, "the estimate is of the body that was sent");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. }))
    );

    let system = sent["system"].as_str().unwrap();
    assert!(
        system.starts_with("you are nanus\n\n[nanus context]"),
        "both, in order"
    );
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(&roles[..3], ["user", "assistant", "assistant"]);
    let generated = sent["messages"][1]["content"][0]["text"].as_str().unwrap();
    assert!(generated.starts_with("[nanus working data"));
    assert!(
        !body.contains("OBSOLETE EARLY OUTPUT"),
        "the hidden fragment is absent"
    );
    assert!(body.contains("kept output b") && body.contains("constraint: never touch"));
}

#[test]
fn the_identity_names_the_route_without_the_credential() {
    let mut request = compiled(&session());
    request.reasoning_effort = Some(ReasoningEffort::High);
    let prepared = prepare(&llm(), managed(request)).unwrap();
    let selection = prepared.selection();
    assert_eq!(selection.provider, "anthropic");
    assert_eq!(selection.protocol, PROTOCOL);
    assert_eq!(selection.model, MODEL);
    assert_eq!(selection.effort.as_deref(), Some("high"));
    assert_eq!(selection.epoch, 7);
    assert_eq!(
        selection.endpoint_digest,
        Digest::of(b"https://api.anthropic.com/v1/messages")
    );
    assert!(!format!("{prepared:?}").contains("test-key"));
    let unset = prepare(&llm(), managed(compiled(&session()))).unwrap();
    assert_eq!(
        unset.selection().effort,
        None,
        "an unset effort is not sent"
    );
}

#[tokio::test]
async fn a_reloaded_session_prepares_the_same_body() {
    let session = session();
    let root = tempfile::tempdir().unwrap();
    let store = nanus_adapter_store::JsonlStore::new(root.path())
        .await
        .unwrap();
    nanus_ports::StorePort::save(&store, &session)
        .await
        .unwrap();
    let reloaded = nanus_ports::StorePort::load(&store, session.id())
        .await
        .unwrap();
    let llm = llm();
    let before = prepare(&llm, managed(compiled(&session))).unwrap();
    let after = prepare(&llm, managed(compiled(&reloaded))).unwrap();
    assert_eq!(before.request_digest(), after.request_digest());
    assert_eq!(before.body(), after.body());
}

#[tokio::test]
async fn a_context_manage_call_is_cut_one_byte_over_its_limit_before_parsing() {
    let llm = llm();
    let limit = 16 * 1024;
    for (bytes, cut) in [(limit, false), (limit + 1, true)] {
        let raw = manage_arguments(bytes);
        let blocks = [thinking_block(), tool_block("context_manage", &raw)];
        let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
        let (_, events) = exchange(prepared, messages_stream(&blocks)).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Error(_)))
        );
        if cut {
            assert_eq!(arguments(&events), OVERSIZED_ARGUMENTS);
            assert!(
                replay_of(&events).is_none(),
                "no signed replay can carry it"
            );
        } else {
            assert_eq!(
                arguments(&events),
                raw,
                "at the limit the arguments pass whole"
            );
            let replay = replay_of(&events).unwrap();
            let input: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(replay.blocks[1]["input"], input, "and its replay is whole");
        }
    }
}

#[tokio::test]
async fn arguments_before_their_call_is_named_are_refused_and_recall_has_its_own_limit() {
    let llm = llm();
    // A fragment for a block no tool call opened is what a late name would look like.
    let early = vec![json!({ "type": "input_json_delta", "partial_json": "{\"q\":1}" })];
    let sse = messages_stream(&[(json!({ "type": "text", "text": "" }), early)]);
    let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
    let (_, events) = exchange(prepared, sse).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Error(_)))
    );
    assert!(arguments(&events).is_empty(), "nothing reached a parser");

    let raw = format!(r#"{{"q":"{}"}}"#, "y".repeat(2 * 1024));
    let blocks = [tool_block("context_recall", &raw)];
    let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
    let (_, events) = exchange(prepared, messages_stream(&blocks)).await;
    assert_eq!(arguments(&events), OVERSIZED_ARGUMENTS);

    // Another tool keeps its existing bounds: the same bytes under `read` stream whole.
    let blocks = [tool_block("read", &raw)];
    let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
    let (_, events) = exchange(prepared, messages_stream(&blocks)).await;
    assert_eq!(arguments(&events), raw);
}

/// A replayed assistant turn bound to `prefix_digest`, carrying signed thinking and one call.
fn signed_turn(prefix_digest: String, with_call: bool) -> Message {
    let mut blocks = vec![json!({ "type": "thinking", "thinking": "considering",
        "signature": SIGNATURE })];
    if with_call {
        blocks.push(json!({ "type": "tool_use", "id": "t1", "name": "read",
            "input": { "path": "t1" } }));
    }
    Message::Assistant {
        text: None,
        reasoning: Some("considering".into()),
        tool_calls: if with_call {
            vec![call("t1")]
        } else {
            Vec::new()
        },
        replay: Some(AssistantReplay {
            protocol: PROTOCOL.into(),
            prefix_digest,
            context_receipt: None,
            blocks,
        }),
    }
}

/// The head of a managed conversation with `notice` as its fixed-schema notice.
fn head(notice: &str) -> Vec<Message> {
    vec![
        Message::system("you are nanus"),
        Message::system(notice),
        Message::user("go"),
    ]
}

/// The digest a response to `messages` would carry: the prefix of exactly that request.
fn produced_under(messages: Vec<Message>) -> String {
    let prepared = prepare(&llm(), managed(request(messages))).unwrap();
    crate::wire::request_prefix(&serde_json::from_str(prepared.body()).unwrap())
}

/// T21: an unchanged prefix sends the checked signed blocks; a changed one sends the neutral
/// form; and neither ever carries a signature the prefix did not produce.
#[tokio::test]
async fn signed_replay_is_sent_only_under_the_prefix_that_produced_it() {
    let digest = produced_under(head("[nanus context] revision=1"));
    let follow = |notice: &str| {
        let mut messages = head(notice);
        messages.push(signed_turn(digest.clone(), true));
        messages.push(Message::tool(ToolCallId::new("t1"), "out", false));
        request(messages)
    };

    let unchanged = prepare(&llm(), managed(follow("[nanus context] revision=1"))).unwrap();
    let body = unchanged.body().to_owned();
    let (received, _) = exchange(unchanged, messages_stream(&[])).await;
    assert_eq!(received, body.as_bytes());
    let sent: Value = serde_json::from_slice(&received).unwrap();
    assert_eq!(sent["messages"][1]["content"][0]["signature"], SIGNATURE);

    let changed = prepare(&llm(), managed(follow("[nanus context] revision=2"))).unwrap();
    let sent: Value = serde_json::from_str(changed.body()).unwrap();
    assert!(
        !changed.body().contains(SIGNATURE),
        "no forged or stale signature"
    );
    let blocks = sent["messages"][1]["content"].as_array().unwrap();
    assert!(
        blocks.iter().all(|block| block["type"] != "thinking"),
        "the neutral form has no thinking"
    );
    assert_eq!(sent["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(sent["messages"][1]["content"][0]["id"], "t1");
}

#[test]
fn a_kept_turn_with_only_unadmitted_replay_is_refused_rather_than_dropped() {
    let digest = produced_under(head("[nanus context] revision=1"));
    let follow = |notice: &str| {
        let mut messages = head(notice);
        messages.push(signed_turn(digest.clone(), false));
        messages.push(Message::user("and then"));
        request(messages)
    };
    let unchanged = prepare(&llm(), managed(follow("[nanus context] revision=1"))).unwrap();
    assert!(
        unchanged.body().contains(SIGNATURE),
        "admitted, it is sent as signed"
    );
    match prepare(&llm(), managed(follow("[nanus context] revision=2"))) {
        Err(error) => assert!(
            error.to_string().contains("protocol_incompatible"),
            "{error}"
        ),
        Ok(_) => panic!("a changed prefix leaves this turn nothing to say"),
    }
}

#[test]
fn only_the_official_endpoint_and_adaptive_models_are_supported() {
    let official = llm();
    for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"] {
        assert_eq!(
            official.managed_support(model),
            ManagedSupport::Supported { policy_version: 1 }
        );
    }
    assert_eq!(
        official.managed_support("claude-sonnet-4-20250514"),
        ManagedSupport::Unsupported
    );
    let proxy = AnthropicLlm::new(AnthropicConfig::with_base_url(
        MODEL,
        "test-key",
        "https://proxy.test/v1",
    ))
    .unwrap();
    assert_eq!(proxy.managed_support(MODEL), ManagedSupport::Unsupported);
    let refused = proxy.prepare_managed(managed(compiled(&session())));
    assert!(matches!(refused, Err(error) if error.to_string().contains("unsupported_mode")));
}

/// Expects a preparation refusal that names `code`.
fn refused_with(request: ChatRequest, code: &str) {
    match prepare(&llm(), managed(request)) {
        Err(error) => assert!(error.to_string().contains(code), "{error}"),
        Ok(_) => panic!("expected a {code} refusal"),
    }
}

#[test]
fn the_output_reservation_is_explicit_and_never_clamped() {
    let mut absent = compiled(&session());
    absent.max_tokens = None;
    refused_with(absent, "protocol_incompatible");
    let mut over = compiled(&session());
    over.max_tokens = Some(128_001);
    refused_with(over, "candidate_too_large");
    let mut at = compiled(&session());
    at.max_tokens = Some(128_000);
    let prepared = prepare(&llm(), managed(at)).unwrap();
    assert!(prepared.body().contains("\"max_tokens\":128000"));
}

#[test]
fn a_request_outside_the_managed_grammar_is_refused() {
    let valid = compiled(&session());
    assert!(prepare(&llm(), managed(valid.clone())).is_ok());

    let mut last = valid.clone();
    last.messages.truncate(4);
    refused_with(last, "generated data is never the last message");

    let mut orphan = valid.clone();
    orphan.messages.retain(|message| {
        !matches!(message, Message::Assistant { tool_calls, .. }
        if tool_calls.first().is_some_and(|call| call.id.as_str() == "c"))
    });
    refused_with(orphan, "answers no surviving call");

    let mut unanswered = valid.clone();
    unanswered.messages.retain(
        |message| !matches!(message, Message::Tool { call_id, .. } if call_id.as_str() == "d"),
    );
    refused_with(unanswered, "has no result");

    let mut late_system = valid.clone();
    late_system.messages.push(Message::system("late"));
    refused_with(late_system, "follows the conversation");

    let mut moved = valid;
    let generated = moved.messages.remove(3);
    moved.messages.insert(5, generated);
    refused_with(moved, "directly after the first user message");
}

/// T01: the ordinary path sends the body it always sent, and streams without managed limits.
#[tokio::test]
async fn the_ordinary_request_bytes_and_decoding_are_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let llm = AnthropicLlm::new(AnthropicConfig::with_base_url(MODEL, "k", base)).unwrap();
    let raw = manage_arguments(16 * 1024 + 1);
    let server = serve_once(
        listener,
        messages_stream(&[tool_block("context_manage", &raw)]),
    );
    let request = ChatRequest::new(
        MODEL,
        vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant(None, Some("why".into()), vec![call("c1")]),
            Message::tool(ToolCallId::new("c1"), "out", false),
        ],
    )
    .with_tools(vec![schema("read")]);
    let events: Vec<LlmEvent> = llm.stream_chat(request).collect().await;
    let received = String::from_utf8(server.join().unwrap()).unwrap();
    assert_eq!(received, LEGACY_BODY);
    assert_eq!(
        arguments(&events),
        raw,
        "no managed limit on the ordinary path"
    );
}

/// The body the ordinary path sent for the request above before managed preparation existed.
const LEGACY_BODY: &str = concat!(
    r#"{"max_tokens":64000,"messages":[{"content":[{"text":"hi","type":"text"}],"role":"#,
    r#""user"},{"content":[{"id":"c1","input":{"path":"c1"},"name":"read","type":"tool_"#,
    r#"use"}],"role":"assistant"},{"content":[{"content":"out","is_error":false,"tool_u"#,
    r#"se_id":"c1","type":"tool_result"}],"role":"user"}],"model":"claude-opus-5-5","st"#,
    r#"ream":true,"system":"sys","thinking":{"type":"adaptive"},"tools":[{"description""#,
    r#":"the read tool","input_schema":{"type":"object"},"name":"read"}]}"#,
);
