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

use super::managed::{PROTOCOL, PreparedCall, RESPONSES_PROTOCOL, prepare};
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
        content_blocks: None,
    });
    work(&mut session, "a", "OBSOLETE EARLY OUTPUT");
    work(&mut session, "b", "kept output b");
    session.append(SessionEvent::UserMessage {
        text: "now the second part".into(),
        content_blocks: None,
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
async fn exchange(prepared: PreparedCall, sse: String) -> (Vec<u8>, Vec<LlmEvent>) {
    exchange_at(prepared, "/chat/completions", sse).await
}

/// [`exchange`] at an endpoint path, so the Responses wire's `/responses` is what is hit.
async fn exchange_at(
    mut prepared: PreparedCall,
    path: &str,
    sse: String,
) -> (Vec<u8>, Vec<LlmEvent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    prepared.retarget(format!("http://{address}{path}"));
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

/// Every Responses route the spec does not admit is refused before HTTP, with nothing switched:
/// stateless replay, an exact subscription preference, a subscription whose fallback is chat, an
/// id this vendor does not offer, and a gateway.
#[test]
fn every_refused_responses_route_is_refused_before_any_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();

    // F8: an id this vendor does not offer has no declared ceiling, even routed to chat.
    let automatic = automatic();
    assert_eq!(
        automatic.managed_support(LEGACY_MODEL),
        ManagedSupport::Unsupported
    );
    refused_by(&automatic, with_model(LEGACY_MODEL), "unsupported_mode");

    // A gateway is not the official endpoint, whatever model it names.
    let gateway = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        "https://gateway.example/v1",
    ))
    .unwrap();
    assert_eq!(gateway.managed_support(MODEL), ManagedSupport::Unsupported);
    refused_by(&gateway, with_model(MODEL), "unsupported_mode");

    // An exact preference promises a ceiling the subscription endpoint refuses.
    let mut exact_subscription = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    exact_subscription.set_account_id("account");
    exact_subscription
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    let exact_subscription = OpenAiLlm::new(exact_subscription).unwrap();
    assert_eq!(
        exact_subscription.managed_support(MODEL),
        ManagedSupport::Unsupported
    );
    refused_by(&exact_subscription, with_model(MODEL), "unsupported_mode");

    // An automatic preference whose fallback is chat would send the same ceiling.
    let mut chat_fallback = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    chat_fallback.set_protocol(Protocol::ChatCompletions);
    chat_fallback.set_account_id("account");
    let chat_fallback = OpenAiLlm::new(chat_fallback).unwrap();
    assert_eq!(
        chat_fallback.managed_support(MODEL),
        ManagedSupport::Unsupported
    );
    refused_by(&chat_fallback, with_model(MODEL), "unsupported_mode");

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

/// The Responses wire is admitted on the API plan, automatically or by an exact Responses
/// preference, and on the stock subscription plan.
#[test]
fn the_responses_wire_is_supported_on_the_api_plan_and_the_stock_subscription() {
    let automatic = automatic();
    assert_eq!(
        automatic.managed_support(MODEL),
        ManagedSupport::Supported { policy_version: 1 }
    );
    assert!(automatic.endpoint_for(MODEL).ends_with("/responses"));
    assert!(prepare(&automatic, managed(with_model(MODEL))).is_ok());

    let mut exact = OpenAiConfig::new(Vendor::OpenAi, MODEL, "test-key");
    exact
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    let exact = OpenAiLlm::new(exact).unwrap();
    assert_eq!(
        exact.managed_support(MODEL),
        ManagedSupport::Supported { policy_version: 1 }
    );
    assert!(prepare(&exact, managed(with_model(MODEL))).is_ok());

    let mut subscription = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    subscription.set_protocol(Protocol::Responses);
    subscription.set_account_id("account");
    let subscription = OpenAiLlm::new(subscription).unwrap();
    assert_eq!(
        subscription.managed_support(MODEL),
        ManagedSupport::Supported { policy_version: 1 }
    );
    assert!(prepare(&subscription, managed(with_model(MODEL))).is_ok());
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
fn inception_is_supported_on_its_api_models_only() {
    let api = OpenAiLlm::new(OpenAiConfig::new(
        Vendor::Inception,
        "mercury-3",
        "test-key",
    ))
    .unwrap();
    for model in ["mercury-3", "mercury-2.5", "mercury-2"] {
        assert_eq!(
            api.managed_support(model),
            ManagedSupport::Supported { policy_version: 1 },
            "{model}"
        );
    }
    assert_eq!(
        api.managed_support("mercury-future"),
        ManagedSupport::Unsupported
    );
    let prepared = prepare(&api, managed(with_model("mercury-3"))).expect("managed call prepared");
    let body: Value = serde_json::from_str(prepared.body()).expect("the prepared body is JSON");
    assert_eq!(body["max_completion_tokens"], 4_096);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["reasoning_effort"], "medium");
    assert!(body.get("thinking").is_none());
    let mut over = with_model("mercury-3");
    over.max_tokens = Some(131_073);
    assert!(prepare(&api, managed(over)).is_err());

    let gateway = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::Inception,
        "mercury-3",
        "test-key",
        "https://gateway.invalid/v1",
    ))
    .unwrap();
    assert_eq!(
        gateway.managed_support("mercury-3"),
        ManagedSupport::Unsupported
    );
    refused_by(&gateway, with_model("mercury-3"), "unsupported_mode");
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

/// A session a handoff started, on the managed wire: the seed is a leading system message after the
/// notice on every request, the first included, and a session with no seed sends none.
mod seeded {
    use nanus_domain::context::managed::{
        ContextFrontier, ContextModeRecord, ContextPolicy, HANDOFF_SEED_LABEL, HandoffSeed,
        ModeActor, ModeReason,
    };

    use super::*;

    /// The handoff document a seed carries: the harness header, a blank line, the narrative.
    const DOCUMENT: &str = "source_session: old\nsource_frontier: events=9\ngoal: none\n\
                            context_mode: managed\nmodel: m effort: none\nworkspace: /w\n\
                            date: 2026-10-21\n\n## State\n- the parser is half done";

    /// A session a handoff started: its mode record and seed, then `prompts`, with a tool call
    /// after the first prompt when there is more than one.
    fn seeded_session(prompts: &[&str]) -> Session {
        let mut session = Session::new(SessionId::new("seeded-wire"), 0, "/w");
        session.upgrade_to_managed_body();
        session.append(SessionEvent::ContextMode {
            payload: Box::new(ContextModeRecord {
                policy: ContextPolicy::managed(),
                actor: ModeActor::Human,
                reason: ModeReason::Enable,
                previous_revision: 0,
            }),
        });
        session.append(SessionEvent::ContextSeed {
            payload: Box::new(HandoffSeed {
                text: DOCUMENT.into(),
                document_blake3: Digest::of(DOCUMENT.as_bytes()),
                source_session_id: "old".into(),
                source_frontier: ContextFrontier {
                    session_id: "old".into(),
                    event_count: 9,
                    prefix_blake3: Digest::of(b"prefix"),
                    projection_revision: 0,
                },
            }),
        });
        for (index, prompt) in prompts.iter().enumerate() {
            session.append(SessionEvent::UserMessage {
                text: (*prompt).into(),
                content_blocks: None,
            });
            if index == 0 && prompts.len() > 1 {
                work(&mut session, "a", "seeded output a");
            }
        }
        assert!(
            session.try_to_jsonl().is_ok(),
            "a session the store would write"
        );
        session
    }

    /// The request the runner composes for a seeded session: the system prompt, the notice, the
    /// seed, then the conversation, with nothing hidden.
    fn compiled_seeded(session: &Session) -> ChatRequest {
        let derived = fragments::derive(session.log()).unwrap();
        let protected = state::protected(session.log(), &derived);
        let selection = Selection {
            revision: 0,
            hidden: &[],
            notes: &[],
            notes_goal_revision: None,
        };
        let facts = NoticeFacts {
            input_allowance: 100_000,
            ..NoticeFacts::default()
        };
        let effective =
            derive_effective_context(session, &derived, &protected, selection, None, &facts)
                .unwrap();
        assert!(
            effective.seed.is_some(),
            "a seeded session carries its seed"
        );
        let mut messages = vec![Message::system("you are nanus"), effective.notice];
        messages.extend(effective.seed);
        messages.extend(effective.messages);
        ChatRequest::new(MODEL, messages)
            .with_tools(vec![schema("read"), schema("context_manage")])
            .with_max_tokens(4_096)
    }

    /// The roles and the leading messages of a prepared seeded request.
    fn prepared(request: ChatRequest) -> (Vec<String>, Value, String) {
        let prepared = prepare(&llm(), managed(request)).unwrap();
        let body = prepared.body().to_owned();
        let sent: Value = serde_json::from_str(&body).unwrap();
        let roles = sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap().to_owned())
            .collect();
        (roles, sent["messages"].clone(), body)
    }

    #[test]
    fn a_seeded_sessions_first_request_carries_the_seed_after_the_notice() {
        let request = compiled_seeded(&seeded_session(&["continue the parser"]));
        let (roles, messages, body) = prepared(request);
        assert_eq!(roles, ["system", "system", "system", "user"]);
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .starts_with("[nanus context]")
        );
        let seed = messages[2]["content"].as_str().unwrap();
        assert!(
            seed.starts_with(HANDOFF_SEED_LABEL) && seed.ends_with(DOCUMENT),
            "{seed}"
        );
        assert_eq!(messages[3]["content"], json!("continue the parser"));
        assert!(!body.contains("[nanus working data"));
    }

    #[test]
    fn a_seeded_sessions_later_request_still_leads_with_the_seed() {
        let request = compiled_seeded(&seeded_session(&["first", "second"]));
        let (roles, messages, _) = prepared(request);
        assert_eq!(&roles[..4], ["system", "system", "system", "user"]);
        assert!(messages[2]["content"].as_str().unwrap().ends_with(DOCUMENT));
        assert_eq!(roles.last().map(String::as_str), Some("user"));
    }

    #[test]
    fn a_managed_session_with_no_seed_sends_no_seed() {
        let request = compiled(&session());
        let (roles, _, body) = prepared(request);
        assert_eq!(&roles[..3], ["system", "system", "user"]);
        assert!(!body.contains(HANDOFF_SEED_LABEL));
    }
}

/// The JSON body a prepared call sends, parsed back.
fn sent(prepared: &PreparedCall) -> Value {
    serde_json::from_str(prepared.body()).expect("the body is JSON")
}

/// On the API plan a managed Responses body is the ordinary encoding with plain history: the
/// reservation is sent as the output ceiling, the prompt is `instructions`, and no replay field
/// or reasoning item is present.
#[test]
fn a_managed_responses_body_on_the_api_plan_is_plain_and_sends_its_reservation() {
    let llm = automatic();
    let prepared = prepare(&llm, managed(with_model(MODEL))).unwrap();
    let body = sent(&prepared);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["max_output_tokens"], json!(4_096));
    assert_eq!(prepared.estimate().reservation, 4_096);
    assert!(
        body["instructions"]
            .as_str()
            .is_some_and(|text| text.starts_with("you are nanus")),
        "the prompt leads the instructions"
    );
    assert!(body.get("include").is_none());
    assert!(body.get("previous_response_id").is_none());
    let items = body["input"].as_array().expect("input items");
    assert!(
        items.iter().all(|item| item["type"] != "reasoning"),
        "no reasoning item is sent back"
    );
    assert_eq!(prepared.selection().protocol, RESPONSES_PROTOCOL);
}

/// On the stock subscription plan the reservation is charged but never sent, because that plan
/// refuses `max_output_tokens`. A candidate that fits only without the reservation is refused,
/// and a reservation above the declared ceiling is refused rather than clamped.
#[test]
fn the_subscription_plan_omits_the_reservation_and_still_charges_it() {
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    let llm = OpenAiLlm::new(config).unwrap();
    let prepared = prepare(&llm, managed(with_model(MODEL))).unwrap();
    let body = sent(&prepared);
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(prepared.estimate().reservation, 4_096);

    let above = with_model(MODEL).with_max_tokens(128_001);
    refused_by(&llm, above, "candidate_too_large");
    assert!(
        llm.prepare_managed(managed(with_model(MODEL).with_max_tokens(128_000)))
            .is_ok(),
        "the declared ceiling itself is admitted"
    );
}

/// Each offered id declares the same text limits on every wire and on both official endpoints,
/// so a budget is the model's whichever plan pays for it.
#[test]
fn every_offered_id_declares_the_same_limits_on_every_wire() {
    for model in Vendor::OpenAi.models() {
        let reference = automatic().capabilities(model);
        for protocol in [Protocol::ChatCompletions, Protocol::Responses] {
            let mut config = OpenAiConfig::new(Vendor::OpenAi, *model, "test-key");
            config
                .set_protocol_preference(ProtocolPreference::Exact(protocol))
                .unwrap();
            let caps = OpenAiLlm::new(config).unwrap().capabilities(model);
            assert_eq!(caps.context_window_tokens, reference.context_window_tokens);
            assert_eq!(caps.max_input_tokens, reference.max_input_tokens);
            assert_eq!(caps.max_output_tokens, reference.max_output_tokens);
        }
    }
}

/// A Responses body answering `text` and completing.
fn responses_answer(text: &str) -> String {
    let mut sse = frame(&json!({ "type": "response.output_text.delta", "delta": text }));
    sse.push_str(&frame(
        &json!({ "type": "response.completed", "response": {} }),
    ));
    sse
}

/// The subscription plan's configuration: the stock automatic routing, which sends no ceiling.
fn subscription() -> OpenAiLlm {
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        MODEL,
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    OpenAiLlm::new(config).unwrap()
}

/// The exact bytes a managed Responses call sends reach the server, with the reservation as the
/// output ceiling, plain history, and no replay field.
#[tokio::test]
async fn a_managed_responses_request_sends_exactly_the_bytes_it_prepared() {
    let prepared = prepare(&automatic(), managed(with_model(MODEL))).unwrap();
    let body = prepared.body().to_owned();
    let digest = prepared.request_digest().clone();
    let (received, events) = exchange_at(prepared, "/responses", responses_answer("ok")).await;
    assert_eq!(
        received,
        body.as_bytes(),
        "the server got the prepared body"
    );
    assert_eq!(Digest::of(&received), digest);
    let sent: Value = serde_json::from_slice(&received).unwrap();
    assert_eq!(sent["store"], json!(false));
    assert_eq!(sent["max_output_tokens"], json!(4_096));
    assert!(sent.get("include").is_none());
    assert!(sent.get("previous_response_id").is_none());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "ok"))
    );
}

/// The subscription plan's body reaches the server with no ceiling, the reservation being
/// charged by accounting alone.
#[tokio::test]
async fn the_subscription_plan_sends_no_ceiling_to_the_server() {
    let prepared = prepare(&subscription(), managed(with_model(MODEL))).unwrap();
    let body = prepared.body().to_owned();
    let (received, _) = exchange_at(prepared, "/responses", responses_answer("ok")).await;
    assert_eq!(received, body.as_bytes());
    let sent: Value = serde_json::from_slice(&received).unwrap();
    assert!(sent.get("max_output_tokens").is_none());
}

/// A session reloaded from its log prepares the same bytes and digest.
#[test]
fn a_reloaded_responses_session_prepares_the_same_digest() {
    let original = session();
    let reloaded = Session::from_jsonl(&original.try_to_jsonl().unwrap()).unwrap();
    let llm = automatic();
    let first = prepare(&llm, managed(compiled(&original))).unwrap();
    let second = prepare(&llm, managed(compiled(&reloaded))).unwrap();
    assert_eq!(first.body(), second.body());
    assert_eq!(first.request_digest(), second.request_digest());
}

/// An image turn is admitted on Responses, where the model declares image support, and refused on
/// exact chat, where image support is unknown.
#[test]
fn an_image_turn_is_admitted_on_responses_and_refused_on_exact_chat() {
    use base64::Engine as _;
    use nanus_domain::ContentBlock;
    let pixels = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
    let turn = Message::user_with_content(vec![
        ContentBlock::Text("what is in this picture?".into()),
        ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(pixels),
        },
    ])
    .unwrap();
    let request = ChatRequest::new(MODEL, vec![Message::system("you are nanus"), turn])
        .with_max_tokens(4_096);

    let prepared = prepare(&automatic(), managed(request.clone())).unwrap();
    assert!(prepared.body().contains("input_image"), "the image is sent");
    assert!(
        prepare(&llm(), managed(request)).is_err(),
        "exact chat refuses images"
    );
}

/// A `context_manage` call over its limit is cut on the Responses wire even though its name
/// arrives only with the `done` event.
#[tokio::test]
async fn a_responses_call_over_its_limit_is_cut_on_the_wire() {
    let big = "x".repeat(16 * 1024 + 1);
    let mut sse = frame(&json!({ "type": "response.output_item.added", "item": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "arguments": "" } }));
    sse.push_str(&frame(
        &json!({ "type": "response.function_call_arguments.delta",
        "item_id": "fc_1", "delta": big }),
    ));
    sse.push_str(&frame(
        &json!({ "type": "response.output_item.done", "item": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1",
        "name": "context_manage", "arguments": big } }),
    ));
    sse.push_str(&frame(
        &json!({ "type": "response.completed", "response": {} }),
    ));
    let prepared = prepare(&automatic(), managed(with_model(MODEL))).unwrap();
    let (_, events) = exchange_at(prepared, "/responses", sse).await;
    assert_eq!(arguments(&events), OVERSIZED_ARGUMENTS);
}

/// An assistant turn recorded with another protocol's replay blocks, as an Anthropic turn is.
fn anthropic_turn(text: Option<&str>, calls: Vec<ToolCall>) -> Message {
    Message::Assistant {
        text: text.map(str::to_owned),
        reasoning: None,
        replay: Some(nanus_domain::message::AssistantReplay {
            protocol: "anthropic.messages".into(),
            prefix_digest: "0".repeat(64),
            context_receipt: None,
            blocks: vec![json!({ "type": "thinking", "thinking": "hmm", "signature": "sig-1" })],
        }),
        tool_calls: calls,
    }
}

/// Case 3: history is plain on the Responses wire. A replay turn is sent as its text and calls,
/// never as its replay blocks, and a turn only replay can express is refused before HTTP.
#[test]
fn history_is_sent_plain_and_a_replay_only_turn_is_refused() {
    let mut request = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::user("first"),
            anthropic_turn(Some("the answer"), Vec::new()),
            Message::user("second"),
        ],
    )
    .with_max_tokens(4_096);
    let prepared = prepare(&automatic(), managed(request.clone())).unwrap();
    let body = prepared.body();
    assert!(body.contains("the answer"), "the text is sent");
    assert!(!body.contains("sig-1"), "no signed block is sent");
    let items: Value = serde_json::from_str(body).unwrap();
    assert!(
        items["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "reasoning"),
        "no reasoning item"
    );
    assert!(!body.contains("encrypted"), "no encrypted replay");

    // A stateless Responses replay turn: its encrypted items are not sent, only its text and calls.
    let responses_turn = Message::Assistant {
        text: Some("the stateless answer".into()),
        reasoning: None,
        replay: Some(nanus_domain::message::AssistantReplay {
            protocol: "openai.responses".into(),
            prefix_digest: "0".repeat(64),
            context_receipt: None,
            blocks: vec![
                json!({ "id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "opaque-1" }),
            ],
        }),
        tool_calls: Vec::new(),
    };
    let stateless = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::user("first"),
            responses_turn,
            Message::user("second"),
        ],
    )
    .with_max_tokens(4_096);
    let stateless_body = prepare(&automatic(), managed(stateless)).unwrap();
    assert!(stateless_body.body().contains("the stateless answer"));
    assert!(
        !stateless_body.body().contains("opaque-1"),
        "no encrypted item is sent"
    );

    let replay_only = anthropic_turn(None, Vec::new());
    request.messages = vec![
        Message::system("you are nanus"),
        Message::user("first"),
        replay_only,
    ];
    refused_by(&automatic(), request, "protocol_incompatible");
}

/// Case 4: the generated working-data message sits directly after the first user item, as an
/// assistant item, and is refused when it is the last message.
#[test]
fn the_generated_message_is_in_place_and_never_last() {
    let generated = || Message::Assistant {
        text: Some(format!(
            "{}\n- a note",
            nanus_domain::context::managed::compile::MEMORY_LABEL
        )),
        reasoning: None,
        replay: None,
        tool_calls: Vec::new(),
    };
    let placed = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::user("first"),
            generated(),
            Message::user("second"),
        ],
    )
    .with_max_tokens(4_096);
    let prepared = prepare(&automatic(), managed(placed)).unwrap();
    let body = sent(&prepared);
    assert_eq!(
        body["input"][1]["role"],
        json!("assistant"),
        "after the first user item"
    );

    let last = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::user("first"),
            generated(),
        ],
    )
    .with_max_tokens(4_096);
    refused_by(&automatic(), last, "protocol_incompatible");
}

/// Case 5: the seed is a system message and joins `instructions` after the notice; a session with
/// no seed carries none of it.
#[test]
fn a_seed_leads_the_instructions_after_the_notice() {
    let seeded = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::system("the notice"),
            Message::system("SEED: continue the parser"),
            Message::user("hi"),
        ],
    )
    .with_max_tokens(4_096);
    let body = sent(&prepare(&automatic(), managed(seeded)).unwrap());
    assert_eq!(
        body["instructions"],
        json!("you are nanus\n\nthe notice\n\nSEED: continue the parser")
    );

    let unseeded = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::system("the notice"),
            Message::user("hi"),
        ],
    )
    .with_max_tokens(4_096);
    let body = sent(&prepare(&automatic(), managed(unseeded)).unwrap());
    assert_eq!(body["instructions"], json!("you are nanus\n\nthe notice"));
}

/// Case 6, tool results: an image result is answered with its label, and its pixels follow as one
/// trailing user item once the whole group of results has been answered.
#[test]
fn an_image_tool_result_is_answered_with_its_label_and_its_pixels_follow() {
    use base64::Engine as _;
    let pixels = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
    let call = ToolCall::new(
        ToolCallId::new("look"),
        ToolName::new("read").unwrap(),
        json!({ "path": "pic.png" }),
    );
    let request = ChatRequest::new(
        MODEL,
        vec![
            Message::system("you are nanus"),
            Message::user("look at the picture"),
            Message::Assistant {
                text: None,
                reasoning: None,
                replay: None,
                tool_calls: vec![call],
            },
            Message::Tool {
                call_id: ToolCallId::new("look"),
                content: "the picture".into(),
                content_blocks: Some(vec![nanus_domain::ContentBlock::Image {
                    media_type: "image/png".into(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(pixels),
                }]),
                is_error: false,
            },
        ],
    )
    .with_tools(vec![schema("read")])
    .with_max_tokens(4_096);
    let body = sent(&prepare(&automatic(), managed(request)).unwrap());
    let items = body["input"].as_array().unwrap();
    let output = items
        .iter()
        .position(|item| item["type"] == json!("function_call_output"))
        .expect("a result item");
    assert_eq!(
        items[output]["output"],
        json!("[tool image attachment: look; is_error=false]"),
        "the result is answered with its label"
    );
    let pixels_item = &items[items.len() - 1];
    assert_eq!(pixels_item["role"], json!("user"));
    assert!(
        pixels_item.to_string().contains("input_image"),
        "the pixels follow the group"
    );
}
