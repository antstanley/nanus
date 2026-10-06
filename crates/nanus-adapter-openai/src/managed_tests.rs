//! Managed preparation over a real socket, and its refusal on every Responses route.
//!
//! The request each fixture prepares is compiled by the domain's own managed compiler from a
//! session — two leading system messages, the generated working-data message after the first
//! user message, and one hidden fragment absent — so the grammar under test is the one the
//! runner produces. The server captures the body it received, which is what lets the
//! assertions be about what was sent rather than what was encoded.

// Fixtures compute exact byte boundaries (a limit and one byte over it) on small, known sizes.
#![allow(clippy::arithmetic_side_effects, clippy::integer_division)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use futures::StreamExt as _;
use nanus_domain::context::managed::compile::{NoticeFacts, Selection, derive_effective_context};
use nanus_domain::context::managed::{Digest, FragmentId, fragments, state};
use nanus_domain::{
    Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName, ToolSchema,
};
use nanus_ports::{
    ChatRequest, LlmEvent, LlmPort as _, ManagedRequest, ManagedSupport, OVERSIZED_ARGUMENTS,
    PreparedModelCall as _,
};
use serde_json::{Value, json};

use super::managed::{PROTOCOL, PreparedChat, prepare};
use crate::{
    OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor,
    ZAI_BASE_URL, ZAI_CODING_BASE_URL,
};

/// An offered `OpenAI` model, sent to chat by an exact chat preference.
const MODEL: &str = "gpt-5.6-sol";

/// A model before the `gpt-5.6` generation: automatic routing sends it to chat, but it is not one
/// this vendor offers, so it has no declared output ceiling and managed context refuses it.
const LEGACY_MODEL: &str = "gpt-5";

fn work(session: &mut Session, id: &str, output: &str) {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("read").unwrap(),
            json!({ "path": id }),
        )],
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

fn llm() -> OpenAiLlm {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, MODEL, "test-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
        .unwrap();
    OpenAiLlm::new(config).unwrap()
}

fn automatic() -> OpenAiLlm {
    OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, MODEL, "test-key")).unwrap()
}

fn zai() -> OpenAiLlm {
    OpenAiLlm::new(OpenAiConfig::new(Vendor::Zai, "glm-5.2", "test-key")).unwrap()
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

fn frame(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

/// An SSE body whose one tool call arrives as `deltas`, then finishes.
fn tool_stream(deltas: &[Value]) -> String {
    let mut sse = String::new();
    for delta in deltas {
        sse.push_str(&frame(
            &json!({ "choices": [{ "delta": { "tool_calls": [delta] } }] }),
        ));
    }
    sse.push_str(&frame(
        &json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
    ));
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// Sends a prepared call to a fixture server, proving nothing connects before the first poll.
async fn exchange(mut prepared: PreparedChat, sse: String) -> (Vec<u8>, Vec<LlmEvent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    prepared.retarget(format!("http://{address}/chat/completions"));
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

/// The arguments the decoded stream reported for its one tool call.
fn arguments(events: &[LlmEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } => Some(arguments_delta.clone()),
            _ => None,
        })
        .unwrap()
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

/// Prepares, sends and checks that the server got exactly the prepared bytes.
async fn sends_exactly_what_it_prepared(llm: &OpenAiLlm, request: ChatRequest) -> Value {
    let prepared = prepare(llm, managed(request.clone())).unwrap();
    let body = prepared.body().to_owned();
    let digest = prepared.request_digest().clone();
    let estimate = prepared.estimate();
    let (received, events) = exchange(prepared, tool_stream(&[])).await;

    assert_eq!(
        received,
        body.as_bytes(),
        "the server got the prepared body"
    );
    assert_eq!(Digest::of(&received), digest);
    let sent: Value = serde_json::from_slice(&received).unwrap();
    let caps = llm.capabilities(&request.model);
    let again = nanus_ports::capabilities::estimate_managed_payload(caps, &request, &sent).unwrap();
    assert_eq!(again, estimate, "the estimate is of the body that was sent");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. }))
    );
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        &roles[..5],
        ["system", "system", "user", "assistant", "assistant"]
    );
    let generated = sent["messages"][3]["content"].as_str().unwrap();
    assert!(generated.starts_with("[nanus working data"));
    assert!(
        !body.contains("OBSOLETE EARLY OUTPUT"),
        "the hidden fragment is absent"
    );
    assert!(body.contains("kept output b") && body.contains("constraint: never touch"));
    sent
}

#[tokio::test]
async fn a_managed_openai_chat_request_sends_exactly_the_bytes_it_prepared() {
    let sent = sends_exactly_what_it_prepared(&llm(), compiled(&session())).await;
    assert_eq!(sent["max_completion_tokens"], json!(4_096));
}

#[tokio::test]
async fn a_managed_zai_request_sends_exactly_the_bytes_it_prepared() {
    let mut request = compiled(&session());
    request.model = "glm-5.2".into();
    let sent = sends_exactly_what_it_prepared(&zai(), request).await;
    assert_eq!(sent["max_tokens"], json!(4_096));
}

#[test]
fn the_identity_names_the_route_without_the_credential() {
    let prepared = prepare(&llm(), managed(compiled(&session()))).unwrap();
    let selection = prepared.selection();
    assert_eq!(selection.provider, "openai");
    assert_eq!(selection.protocol, PROTOCOL);
    assert_eq!(selection.model, MODEL);
    assert_eq!(selection.effort.as_deref(), Some("medium"));
    assert_eq!(selection.epoch, 7);
    assert_eq!(
        selection.endpoint_digest,
        Digest::of(b"https://api.openai.com/v1/chat/completions")
    );
    assert!(!format!("{prepared:?}").contains("test-key"));

    let mut request = compiled(&session());
    request.model = "glm-5.2".into();
    let prepared = prepare(&zai(), managed(request)).unwrap();
    assert_eq!(prepared.selection().provider, "zai");
    assert_eq!(prepared.selection().effort.as_deref(), Some("max"));
    assert_eq!(
        prepared.selection().endpoint_digest,
        Digest::of(format!("{ZAI_BASE_URL}/chat/completions").as_bytes())
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
        let delta = json!({ "index": 0, "id": "m1",
            "function": { "name": "context_manage", "arguments": raw } });
        let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
        let (_, events) = exchange(prepared, tool_stream(&[delta])).await;
        let reported = arguments(&events);
        if cut {
            assert_eq!(reported, OVERSIZED_ARGUMENTS);
        } else {
            assert_eq!(reported, raw, "at the limit the arguments pass whole");
        }
    }
}

#[tokio::test]
async fn a_name_that_arrives_after_its_arguments_is_still_held_to_the_limit() {
    let llm = llm();
    let raw = format!(r#"{{"q":"{}"}}"#, "y".repeat(2 * 1024));
    let (first, second) = raw.split_at(raw.len() / 2);
    let deltas = [
        json!({ "index": 0, "id": "r1", "function": { "arguments": first } }),
        json!({ "index": 0, "function": { "arguments": second } }),
        json!({ "index": 0, "function": { "name": "context_recall" } }),
    ];
    let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
    let (_, events) = exchange(prepared, tool_stream(&deltas)).await;
    assert_eq!(arguments(&events), OVERSIZED_ARGUMENTS);

    // Another tool keeps its existing bounds: the same bytes under `read` pass whole.
    let deltas = [json!({ "index": 0, "id": "w1",
        "function": { "name": "read", "arguments": raw } })];
    let prepared = prepare(&llm, managed(compiled(&session()))).unwrap();
    let (_, events) = exchange(prepared, tool_stream(&deltas)).await;
    assert_eq!(arguments(&events), raw);
}

/// Expects a preparation refusal that names `code`, from `llm`.
fn refused_by(llm: &OpenAiLlm, request: ChatRequest, code: &str) {
    match prepare(llm, managed(request)) {
        Err(error) => assert!(error.to_string().contains(code), "{error}"),
        Ok(_) => panic!("expected a {code} refusal"),
    }
}

fn refused_with(request: ChatRequest, code: &str) {
    refused_by(&llm(), request, code);
}

fn with_model(model: &str) -> ChatRequest {
    let mut request = compiled(&session());
    request.model = model.into();
    request
}

/// T22: every Responses route is unsupported and refused before HTTP, with nothing switched.
#[test]
fn every_responses_route_is_unsupported_and_refused_before_any_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let automatic = automatic();
    // F8: an id this vendor does not offer has no declared ceiling, even routed to chat.
    assert_eq!(
        automatic.managed_support(LEGACY_MODEL),
        ManagedSupport::Unsupported
    );
    refused_by(&automatic, with_model(LEGACY_MODEL), "unsupported_mode");
    assert_eq!(
        automatic.managed_support("gpt-5.6-sol"),
        ManagedSupport::Unsupported
    );
    refused_by(&automatic, with_model("gpt-5.6-sol"), "unsupported_mode");
    assert!(
        automatic
            .endpoint_for("gpt-5.6-sol")
            .ends_with("/responses"),
        "no wire switch"
    );
    assert_eq!(
        automatic.config().reasoning_effort(),
        nanus_ports::ReasoningEffort::Medium
    );

    let mut subscription = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-5.6-sol",
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    subscription.set_protocol(Protocol::Responses);
    subscription.set_account_id("account");
    let subscription = OpenAiLlm::new(subscription).unwrap();
    for model in ["gpt-5.6-sol", MODEL] {
        assert_eq!(
            subscription.managed_support(model),
            ManagedSupport::Unsupported
        );
        refused_by(&subscription, with_model(model), "unsupported_mode");
    }

    let mut exact = OpenAiConfig::new(Vendor::OpenAi, MODEL, "test-key");
    exact
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    let exact = OpenAiLlm::new(exact).unwrap();
    assert_eq!(exact.managed_support(MODEL), ManagedSupport::Unsupported);
    refused_by(&exact, compiled(&session()), "unsupported_mode");

    let mut stateless = OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "test-key");
    stateless
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    stateless.set_stateless_responses(true);
    stateless.set_response_limits(
        nanus_ports::ResponseLimits::new(8192, 8192, 262_144, 100, 16, 1024).unwrap(),
    );
    let stateless = OpenAiLlm::new(stateless).unwrap();
    assert_eq!(
        stateless.managed_support("gpt-6-astra"),
        ManagedSupport::Unsupported
    );
    let refused = stateless.prepare_managed(managed(with_model("gpt-6-astra")));
    assert!(matches!(refused, Err(error) if error.to_string().contains("unsupported_mode")));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "nothing was sent"
    );
}

#[test]
fn an_exact_chat_preference_is_supported_only_where_tools_are_not_refused() {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-5.6-sol", "test-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
        .unwrap();
    let chat = OpenAiLlm::new(config).unwrap();
    assert_eq!(
        chat.managed_support("gpt-5.6-sol"),
        ManagedSupport::Supported { policy_version: 1 }
    );
    assert!(prepare(&chat, managed(with_model("gpt-5.6-sol"))).is_ok());
    // The flagship's chat wire refuses function tools, so a managed session cannot run there.
    assert_eq!(
        chat.managed_support("gpt-6-astra"),
        ManagedSupport::Unsupported
    );
    refused_by(&chat, with_model("gpt-6-astra"), "unsupported_mode");
}

#[test]
fn zai_is_supported_on_its_api_models_only() {
    let api = zai();
    assert_eq!(
        api.managed_support("glm-5.3"),
        ManagedSupport::Supported { policy_version: 1 }
    );
    assert_eq!(api.managed_support("glm-4.5"), ManagedSupport::Unsupported);
    let coding = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::Zai,
        "glm-5.2",
        "test-key",
        ZAI_CODING_BASE_URL,
    ))
    .unwrap();
    assert_eq!(
        coding.managed_support("glm-5.2"),
        ManagedSupport::Unsupported
    );
    refused_by(&coding, with_model("glm-5.2"), "unsupported_mode");
    let proxy = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        "https://proxy.test/v1",
    ))
    .unwrap();
    assert_eq!(proxy.managed_support(MODEL), ManagedSupport::Unsupported);
}

#[test]
fn the_output_reservation_is_explicit_and_within_the_ceiling() {
    let mut absent = compiled(&session());
    absent.max_tokens = None;
    refused_with(absent, "protocol_incompatible");
    let mut over = compiled(&session());
    over.max_tokens = Some(128_001);
    refused_with(over, "candidate_too_large");
    let mut at = compiled(&session());
    at.max_tokens = Some(128_000);
    assert!(prepare(&llm(), managed(at)).is_ok());
    let mut zai_over = with_model("glm-5.2");
    zai_over.max_tokens = Some(131_073);
    assert!(prepare(&zai(), managed(zai_over)).is_err());
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

    // F15: text that opens with the label anywhere but directly after the first user message
    // is the model's own reply, sent as it is; refusing it would let one reply wedge a session.
    let mut moved = valid;
    let generated = moved.messages.remove(3);
    moved.messages.insert(5, generated);
    assert!(
        prepare(&llm(), managed(moved)).is_ok(),
        "a labelled reply elsewhere is sent"
    );
}

/// T01: the ordinary path sends the body it always sent, and decodes without managed limits.
#[tokio::test]
async fn the_ordinary_request_bytes_and_decoding_are_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let config = OpenAiConfig::with_base_url(Vendor::OpenAi, LEGACY_MODEL, "k", base);
    let llm = OpenAiLlm::new(config).unwrap();
    let raw = manage_arguments(16 * 1024 + 1);
    let delta = json!({ "index": 0, "id": "m1",
        "function": { "name": "context_manage", "arguments": raw } });
    let server = serve_once(listener, tool_stream(&[delta]));
    let request = ChatRequest::new(
        LEGACY_MODEL,
        vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant(
                None,
                Some("why".into()),
                vec![ToolCall::new(
                    ToolCallId::new("c1"),
                    ToolName::new("read").unwrap(),
                    json!({ "path": "a" }),
                )],
            ),
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
    r#"{"max_completion_tokens":128000,"messages":[{"content":"sys","role":"system"},{""#,
    r#"content":"hi","role":"user"},{"content":"","role":"assistant","tool_calls":[{"fu"#,
    r#"nction":{"arguments":"{\"path\":\"a\"}","name":"read"},"id":"c1","type":"functio"#,
    r#"n"}]},{"content":"out","role":"tool","tool_call_id":"c1"}],"model":"gpt-5","reas"#,
    r#"oning_effort":"medium","stream":true,"stream_options":{"include_usage":true},"to"#,
    r#"ols":[{"function":{"description":"the read tool","name":"read","parameters":{"ty"#,
    r#"pe":"object"}},"type":"function"}]}"#,
);
