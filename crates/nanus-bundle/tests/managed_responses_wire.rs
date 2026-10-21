#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

//! The managed `OpenAI` Responses wire inside a runner: the real adapter, served from a local
//! listener, for a managed turn on each plan, a handoff draft, a switch, and a plan switch.
//!
//! The listener answers each request in turn with the next SSE body and keeps what it received,
//! so the assertions are about the bytes that reached the server, not what was encoded.

#[path = "managed_context/harness.rs"]
mod harness;

use std::future::Future;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::rc::Rc;
use std::thread::JoinHandle;
use std::time::Duration;

use harness::{Disk, context, host, session};
use nanus_adapter_openai::{
    OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol,
    ProtocolPreference, Vendor,
};
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::context::managed::{ContextPolicy, ErrorCode, ModeActor};
use nanus_domain::{AgentConfig, SandboxMode, Session, SessionEvent, ToolRegistry};
use nanus_ports::{ClockPort, LlmPort};
use serde_json::{Value, json};

/// Drives a future on the kernel's runtime. The adapter's HTTP client needs a Tokio reactor, which
/// a plain executor does not provide.
fn block<F: Future>(future: F) -> F::Output {
    nanus_kernel::runtime::block_on(future)
}

/// Reads one HTTP request and returns its body.
fn read_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let length = loop {
        let count = stream.read(&mut chunk).unwrap();
        received.extend_from_slice(&chunk[..count]);
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end.saturating_add(4), length);
        }
    };
    let (start, length) = length;
    while received.len() < start.saturating_add(length) {
        let count = stream.read(&mut chunk).unwrap();
        received.extend_from_slice(&chunk[..count]);
    }
    received[start..start.saturating_add(length)].to_vec()
}

/// Answers each request, in order, with the next body, and returns what each request carried.
fn serve(listener: TcpListener, answers: Vec<String>) -> JoinHandle<Vec<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut received = Vec::new();
        for sse in answers {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            received.push(read_body(&mut stream));
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                sse.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(sse.as_bytes()).unwrap();
        }
        received
    })
}

/// A Responses body that says `text` and completes.
fn answer(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        json!({ "type": "response.output_text.delta", "delta": text }),
        json!({ "type": "response.completed", "response": {} })
    )
}

/// A listener on an ephemeral port, and the origin a test points the adapter at.
fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    (listener, origin)
}

/// The stock API plan: automatic routing sends `gpt-5.6-sol` to Responses.
fn api_llm(origin: &str) -> OpenAiLlm {
    let config =
        OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-5.6-sol", "test-key", OPENAI_BASE_URL);
    OpenAiLlm::new(config).unwrap().with_test_origin(origin)
}

/// The `subscription` plan under its stock automatic routing, which sends no ceiling.
fn subscription_llm(origin: &str) -> OpenAiLlm {
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-5.6-sol",
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    OpenAiLlm::new(config).unwrap().with_test_origin(origin)
}

/// The subscription plan under an exact Responses preference, which managed context refuses.
fn exact_subscription_llm(origin: &str) -> OpenAiLlm {
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-5.6-sol",
        "test-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_account_id("account");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    OpenAiLlm::new(config).unwrap().with_test_origin(origin)
}

/// The `OpenAI` adapter with the output ceiling a test chooses: a model that declares 393,216 and a
/// 1,048,576 window (as `DeepSeek` does), or none. Everything else, including the managed route, is the real adapter's.
struct Declared {
    inner: OpenAiLlm,
    ceiling: Option<u32>,
}

impl LlmPort for Declared {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn capabilities(&self, _model: &str) -> nanus_ports::ModelCapabilities {
        nanus_ports::ModelCapabilities {
            context_window_tokens: self.ceiling.map(|_| 1_048_576),
            max_output_tokens: self.ceiling,
            ..nanus_ports::ModelCapabilities::default()
        }
    }

    fn tool_call_support(
        &self,
        model: &str,
        effort: Option<nanus_ports::ReasoningEffort>,
    ) -> nanus_ports::ToolCallSupport {
        self.inner.tool_call_support(model, effort)
    }

    fn stream_chat(&self, request: nanus_ports::ChatRequest) -> nanus_ports::LlmStream {
        self.inner.stream_chat(request)
    }

    fn managed_support(&self, model: &str) -> nanus_ports::ManagedSupport {
        self.inner.managed_support(model)
    }

    fn prepare_managed(
        &self,
        request: nanus_ports::ManagedRequest,
    ) -> nanus_ports::LlmResult<Box<dyn nanus_ports::PreparedModelCall>> {
        self.inner.prepare_managed(request)
    }
}

/// A runner over a [`Declared`] model, fitted to the model's window.
fn declared_runner(origin: &str, ceiling: Option<u32>) -> AgentRunner {
    let inner = api_llm(origin);
    let config = AgentConfig::new(8, 1, "gpt-5.6-sol", 32_768)
        .unwrap()
        .with_context_budget(64_000)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(Declared { inner, ceiling }));
    AgentRunner::new(
        llm,
        ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_model_context_budget()
}

/// The clock every runner in these tests reads.
struct Clock;

impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        1_000
    }
}

/// A runner over `llm` that fits requests to the model's window, as stock composition does.
fn runner_over(llm: OpenAiLlm) -> AgentRunner {
    let config = AgentConfig::new(8, 1, "gpt-5.6-sol", 32_768)
        .unwrap()
        .with_context_budget(64_000)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(llm));
    AgentRunner::new(
        llm,
        ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_model_context_budget()
}

/// The protocol and endpoint digest of each attempt the session recorded, once per attempt: an
/// attempt writes a record when it starts and another when it settles.
fn attempts(session: &Session) -> Vec<(String, String)> {
    let mut seen: Vec<String> = Vec::new();
    let mut found = Vec::new();
    for event in session.log().events() {
        if let SessionEvent::RequestAttempt { payload } = event
            && !seen.contains(&payload.attempt_id)
        {
            seen.push(payload.attempt_id.clone());
            found.push((
                payload.selection.protocol.clone(),
                payload.selection.endpoint_digest.as_str().to_owned(),
            ));
        }
    }
    found
}

/// Enables managed context with the following reservation, through `runner`'s checkpoint.
fn enable(runner: &AgentRunner, session: &mut Session, disk: &Disk) {
    let context = context();
    block(runner.set_context_policy(
        session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(disk, &context).runtime,
    ))
    .unwrap();
}

/// Runs one turn of `text` through `runner`.
fn turn(runner: &AgentRunner, session: &mut Session, disk: &Disk, text: &str) {
    let context = context();
    let outcome =
        block(runner.run_turn_with_runtime(session, text, &mut Silent, host(disk, &context)));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
}

/// Acceptance 14: a managed turn on the API plan reserves the model's ceiling, sent as
/// `max_output_tokens`, and records its attempt as Responses.
#[test]
fn a_managed_turn_on_the_api_plan_sends_the_ceiling_and_records_responses() {
    let (listener, origin) = listener();
    let server = serve(listener, vec![answer("ok")]);
    let runner = runner_over(api_llm(&origin));
    let disk = Disk::new();
    let mut session = session();
    enable(&runner, &mut session, &disk);
    turn(&runner, &mut session, &disk, "Summarise.");

    let received = server.join().unwrap();
    let sent: Value = serde_json::from_slice(&received[0]).unwrap();
    assert_eq!(sent["max_output_tokens"], json!(128_000));
    let recorded = attempts(&session);
    assert_eq!(
        recorded.last().map(|(protocol, _)| protocol.as_str()),
        Some("openai.responses")
    );
}

/// Acceptance 14, the subscription half: the same turn sends no ceiling, and the reservation is
/// charged by accounting alone.
#[test]
fn a_managed_turn_on_the_subscription_plan_sends_no_ceiling() {
    let (listener, origin) = listener();
    let server = serve(listener, vec![answer("ok")]);
    let runner = runner_over(subscription_llm(&origin));
    let disk = Disk::new();
    let mut session = session();
    enable(&runner, &mut session, &disk);
    turn(&runner, &mut session, &disk, "Summarise.");

    let received = server.join().unwrap();
    let sent: Value = serde_json::from_slice(&received[0]).unwrap();
    assert!(sent.get("max_output_tokens").is_none());
    assert_eq!(
        attempts(&session)
            .last()
            .map(|(protocol, _)| protocol.as_str()),
        Some("openai.responses")
    );
}

/// Acceptance 15: a handoff draft on the subscription sends no ceiling, and its document says the
/// reservation follows the model.
#[test]
fn a_handoff_draft_on_the_subscription_says_the_reserve_follows_the_model() {
    let (listener, origin) = listener();
    let server = serve(
        listener,
        vec![answer("ok"), answer("## Goal\n- ship the parser")],
    );
    let runner = runner_over(subscription_llm(&origin));
    let disk = Disk::new();
    let context = context();
    let mut session = session();
    enable(&runner, &mut session, &disk);
    turn(&runner, &mut session, &disk, "Summarise.");
    let draft = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).unwrap();

    let received = server.join().unwrap();
    let drafted: Value = serde_json::from_slice(&received[1]).unwrap();
    assert!(drafted.get("max_output_tokens").is_none());
    assert!(
        draft.text.contains("output_reserve=model"),
        "the policy line says so: {}",
        draft.text
    );
}

/// Acceptance 16: a following session enabled where the model declares 393,216 reserves the
/// `OpenAI` model's 128,000 once it is driven by `OpenAI`, and keeps working.
#[test]
fn a_following_reservation_follows_a_switch_from_a_larger_model() {
    let (listener, origin) = listener();
    let server = serve(listener, vec![answer("ok")]);
    let disk = Disk::new();
    let mut session = session();
    let before = declared_runner("http://127.0.0.1:9", Some(393_216));
    enable(&before, &mut session, &disk);
    assert_eq!(
        AgentRunner::context_policy(&session).output_reserve_tokens,
        393_216,
        "the recorded snapshot is the model in force"
    );
    let after = runner_over(api_llm(&origin));
    turn(&after, &mut session, &disk, "Summarise.");

    let received = server.join().unwrap();
    let sent: Value = serde_json::from_slice(&received[0]).unwrap();
    assert_eq!(sent["max_output_tokens"], json!(128_000));
}

/// Acceptance 16, the explicit half: the same switch under an explicit 200,000 makes the session
/// unready with `unsupported_mode`, and the session's selection is not reverted.
#[test]
fn an_explicit_reservation_above_the_new_ceiling_is_unready_and_not_reverted() {
    let disk = Disk::new();
    let context = context();
    let mut session = session();
    let before = declared_runner("http://127.0.0.1:9", Some(393_216));
    block(before.set_context_policy(
        &mut session,
        ContextPolicy {
            output_reserve_tokens: 200_000,
            output_reserve_follows_model: false,
            ..ContextPolicy::managed()
        },
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let recorded = session.clone();
    let after = runner_over(api_llm("http://127.0.0.1:9"));
    let status = after
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert!(!status.managed_ready);
    assert_eq!(status.unavailable_reason, Some(ErrorCode::UnsupportedMode));
    assert_eq!(session, recorded, "the switch changes no record");
}

/// Acceptance 19, the refusal: a following policy on a model that declares no ceiling is refused,
/// and nothing is written.
#[test]
fn enabling_a_following_policy_without_a_declared_ceiling_writes_nothing() {
    let disk = Disk::new();
    let context = context();
    let runner = declared_runner("http://127.0.0.1:9", None);
    let mut session = session();
    let refused = block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert_eq!(
        refused.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::UnsupportedMode)
    );
    assert_eq!(disk.commits(), 0);
    assert!(!session.is_managed_body());
}

/// Acceptance 17: a session managed on the API plan takes its next turn on the subscription plan
/// under a different endpoint, and the exact subscription preference is unready with its code.
#[test]
fn a_plan_switch_changes_the_attempt_endpoint_and_an_exact_subscription_is_unready() {
    // One listener per plan: the test origin stands in for each plan's endpoint, so the digest of
    // the endpoint each attempt names is what tells the two apart.
    let (api_listener, api_origin) = listener();
    let api_server = serve(api_listener, vec![answer("first")]);
    let (sub_listener, sub_origin) = listener();
    let sub_server = serve(sub_listener, vec![answer("second")]);
    let api = runner_over(api_llm(&api_origin));
    let disk = Disk::new();
    let context = context();
    let mut session = session();
    enable(&api, &mut session, &disk);
    turn(&api, &mut session, &disk, "one");
    let subscription = runner_over(subscription_llm(&sub_origin));
    turn(&subscription, &mut session, &disk, "two");
    api_server.join().unwrap();
    sub_server.join().unwrap();
    let recorded = attempts(&session);
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].0, recorded[1].0, "both plans speak Responses");
    assert_ne!(recorded[0].1, recorded[1].1, "the endpoint digest changed");

    let exact = runner_over(exact_subscription_llm("http://127.0.0.1:9"));
    let status = exact
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert!(!status.managed_ready);
    assert_eq!(
        status.unavailable_reason,
        Some(ErrorCode::ProtocolIncompatible)
    );
}

/// Acceptance 21 (the budget half): with the model's window, an exact-chat `OpenAI` runner fits to
/// the window; without it, to the configured 64,000. The legacy prompt budget derives from this.
#[test]
fn an_exact_chat_runner_fits_to_the_model_window_only_when_asked() {
    let mut chat =
        OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-6-astra", "test-key", OPENAI_BASE_URL);
    chat.set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
        .unwrap();
    let llm = OpenAiLlm::new(chat.clone()).unwrap();
    let plain = AgentRunner::new(
        Rc::new(Box::new(llm)),
        ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        AgentConfig::new(8, 1, "gpt-6-astra", 32_768)
            .unwrap()
            .with_context_budget(64_000)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap();
    assert_eq!(plain.context_budget(), 64_000);
    assert_eq!(
        plain.with_model_context_budget().context_budget(),
        1_050_000
    );
}

/// A Chat Completions body answering `text` and stopping.
fn chat_answer(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({ "choices": [{ "index": 0, "delta": { "content": text } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] })
    )
}

/// The `OpenAI` adapter on exact Chat Completions, for the legacy path.
fn exact_chat_llm(origin: &str) -> OpenAiLlm {
    let mut config =
        OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-5.6-sol", "test-key", OPENAI_BASE_URL);
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
        .unwrap();
    OpenAiLlm::new(config).unwrap().with_test_origin(origin)
}

/// Case 20: status reports the reservation in force. A following session reports the new model's
/// reservation after a switch; a legacy session reports what enabling would reserve; a following
/// session on a model with no ceiling reports its recorded snapshot, unready with its code.
#[test]
fn status_reports_the_effective_reservation_across_a_switch_and_for_legacy_sessions() {
    let disk = Disk::new();
    let context = context();

    // A following session, enabled where the model declares 393,216.
    let mut following = session();
    let larger = declared_runner("http://127.0.0.1:9", Some(393_216));
    enable(&larger, &mut following, &disk);
    // One completed turn on the new model, so the session has a snapshot to report.
    let (listener, origin) = listener();
    let server = serve(listener, vec![answer("ok")]);
    let after_switch = runner_over(api_llm(&origin));
    turn(&after_switch, &mut following, &disk, "Summarise.");
    server.join().unwrap();
    let status = after_switch
        .context_status(&following, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(
        status.output_reserve_tokens, 128_000,
        "the new model's reservation"
    );
    assert!(status.managed_ready, "{:?}", status.unavailable_reason);

    let no_ceiling = declared_runner("http://127.0.0.1:9", None);
    let snapshot = no_ceiling
        .context_status(&following, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(
        snapshot.output_reserve_tokens, 393_216,
        "the recorded snapshot"
    );
    assert!(!snapshot.managed_ready);
    assert_eq!(
        snapshot.unavailable_reason,
        Some(ErrorCode::UnsupportedMode)
    );

    // A legacy session with no recorded policy reports what enabling would reserve.
    let legacy = session();
    let would_reserve = after_switch
        .context_status(&legacy, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(would_reserve.output_reserve_tokens, 128_000);
    assert!(!would_reserve.managed_ready, "legacy is not managed");
}

/// Case 21, the legacy request: an exact-chat `OpenAI` runner fitted to the window sends its model's
/// 128,000 as the output ceiling, as it did before, and its prompt is the window's.
#[test]
fn a_legacy_request_on_exact_chat_sends_the_model_ceiling() {
    let (listener, origin) = listener();
    let server = serve(listener, vec![chat_answer("ok")]);
    let runner = runner_over_chat(exact_chat_llm(&origin));
    let disk = Disk::new();
    let mut session = session();
    turn(&runner, &mut session, &disk, "Summarise.");

    let received = server.join().unwrap();
    let sent: Value = serde_json::from_slice(&received[0]).unwrap();
    assert_eq!(sent["max_completion_tokens"], json!(128_000));
    assert!(
        sent.get("max_output_tokens").is_none(),
        "chat sends its own field"
    );
}

/// A runner over exact-chat `OpenAI` that fits to the model's window.
fn runner_over_chat(llm: OpenAiLlm) -> AgentRunner {
    let config = AgentConfig::new(8, 1, "gpt-5.6-sol", 32_768)
        .unwrap()
        .with_context_budget(64_000)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(llm));
    AgentRunner::new(
        llm,
        ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_model_context_budget()
}

/// A runner over exact-chat `OpenAI`, fitted to the model's window when `fitted`, and otherwise to the
/// configured 64,000.
fn chat_runner(llm: OpenAiLlm, fitted: bool) -> AgentRunner {
    let config = AgentConfig::new(8, 1, "gpt-5.6-sol", 32_768)
        .unwrap()
        .with_context_budget(64_000)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(llm));
    let runner = AgentRunner::new(
        llm,
        ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap();
    if fitted {
        runner.with_model_context_budget()
    } else {
        runner
    }
}

/// Case 21, the legacy prompt budget on the wire: a conversation of about 100,000 tokens is sent whole
/// when the runner is fitted to the model's window (691,500 of prompt), and its oldest turn is elided
/// under the configured 64,000.
#[test]
fn a_legacy_conversation_is_sent_whole_only_when_fitted_to_the_window() {
    let long = |marker: &str| format!("{marker}{}", "x".repeat(10_000));
    let mut history = session();
    for turn in 0..40_u32 {
        let question = if turn == 0 {
            long("FIRST-MARK ")
        } else {
            long("")
        };
        history.append(SessionEvent::TurnStart { turn });
        history.append(SessionEvent::UserMessage {
            text: question,
            content_blocks: None,
        });
        history.append(SessionEvent::AssistantMessage {
            replay: None,
            text: Some(long("")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        history.append(SessionEvent::TurnEnd {
            turn,
            reason: nanus_domain::TurnEndReason::Completed,
        });
    }
    let disk = Disk::new();

    let (first_listener, origin) = listener();
    let server = serve(first_listener, vec![chat_answer("ok")]);
    let fitted = chat_runner(exact_chat_llm(&origin), true);
    assert_eq!(fitted.prompt_budget(), 691_500, "922,000 less a quarter");
    let mut whole = history.clone();
    turn(&fitted, &mut whole, &disk, "latest");
    let sent = String::from_utf8_lossy(&server.join().unwrap()[0]).into_owned();
    assert!(
        sent.contains("FIRST-MARK"),
        "the whole conversation is sent"
    );

    let (second_listener, origin) = listener();
    let server = serve(second_listener, vec![chat_answer("ok")]);
    let configured = chat_runner(exact_chat_llm(&origin), false);
    assert_eq!(configured.prompt_budget(), 64_000, "the configured budget");
    let mut trimmed = history;
    turn(&configured, &mut trimmed, &disk, "latest");
    let sent = String::from_utf8_lossy(&server.join().unwrap()[0]).into_owned();
    assert!(
        !sent.contains("FIRST-MARK"),
        "the oldest turn is elided under 64,000"
    );
}
