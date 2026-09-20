//! The wire path over a real socket: HTTP framing, SSE decoding, tool-call reassembly.
//!
//! Everything between `OpenAiLlm::stream_chat` and the events the agent loop consumes is
//! exercised here against a **real TCP server**, not a mock stream. The gap this closes
//! is the one a unit test on the accumulator cannot: that reqwest really sends the body
//! this adapter encodes, with the headers the vendor expects, and that the frames a real
//! `text/event-stream` delivers are decoded into the events the loop expects.
//!
//! The server also **captures the request**, so the assertions are about bytes on the
//! wire rather than about the encoder's return value — which is the difference between
//! testing what was written and testing what was sent.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use nanus_domain::Message;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort as _};
use serde_json::Value;

/// A stub HTTP server that answers every request with one scripted response.
struct Server {
    /// The base URL to point an adapter at.
    base_url: String,
    /// Every request it received, in order, as raw text.
    requests: Arc<Mutex<Vec<String>>>,
}

/// Serves `status` and `body` for every request, recording each request's text.
fn spawn_server(status: u16, body: &str) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let body = body.to_owned();

    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            if serve(stream, status, &body, &seen).is_err() {
                return;
            }
        }
    });

    Server {
        base_url: format!("http://{address}"),
        requests,
    }
}

/// Reads one request, answers it, and records it.
fn serve(
    mut stream: std::net::TcpStream,
    status: u16,
    body: &str,
    seen: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let _ignored = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let request = read_request(&mut stream);
    seen.lock()
        .expect("the request log is not poisoned")
        .push(request);

    let reason = if status == 200 { "OK" } else { "Error" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: text/event-stream\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Reads one HTTP request, returning its raw text.
///
/// The body is read until `content-length` bytes have arrived, which is what keeps the
/// request from being truncated and turning the test into an assertion about this helper.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut expected: Option<usize> = None;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => buffer.extend_from_slice(chunk.get(..count).unwrap_or(&chunk)),
        }
        if expected.is_none() {
            let text = String::from_utf8_lossy(&buffer);
            if let Some((head, _)) = text.split_once("\r\n\r\n") {
                expected = Some(
                    head.lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0),
                );
            }
        }
        if let Some(expected) = expected {
            let text = String::from_utf8_lossy(&buffer);
            if let Some((_, body)) = text.split_once("\r\n\r\n")
                && body.len() >= expected
            {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Builds one SSE frame carrying `payload`.
fn frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

/// Builds the SSE body that streams reasoning, an answer, and a tool call whose arguments
/// arrive in fragments, ending in the sentinel.
fn tool_call_stream() -> String {
    let mut body = String::new();
    body.push_str(&frame(
        r#"{"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#,
    ));
    body.push_str(&frame(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#));
    body.push_str(&frame(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
    ));
    // The arguments continue mid-JSON, as a real stream does.
    body.push_str(&frame(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
    ));
    body.push_str(&frame(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":3}}}"#,
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// Runs one request against `server` with `vendor`, returning every event.
fn collect(vendor: nanus_adapter_openai::Vendor, server: &Server, model: &str) -> Vec<LlmEvent> {
    let config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        vendor,
        model,
        "test-key",
        &server.base_url,
    );
    let llm = nanus_adapter_openai::OpenAiLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(model, vec![Message::user("hi")]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let mut events = Vec::new();
        let mut stream = llm.stream_chat(request);
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

/// Splits a captured request into its head and its decoded body.
fn split_request(request: &str) -> (String, Value) {
    let (head, body) = request
        .split_once("\r\n\r\n")
        .expect("a request has a body");
    let parsed: Value = serde_json::from_str(body).expect("the body is JSON");
    (head.to_owned(), parsed)
}

/// Returns the one request the server was sent.
fn only_request(server: &Server) -> String {
    server
        .requests
        .lock()
        .expect("the request log")
        .first()
        .cloned()
        .expect("a request arrived")
}

/// The whole path for one vendor: the request goes out as this adapter encodes it, and the
/// documented frames come back as the events the loop consumes.
#[test]
fn a_streamed_tool_call_becomes_events_over_a_real_socket() {
    let server = spawn_server(200, &tool_call_stream());
    let events = collect(nanus_adapter_openai::Vendor::OpenAi, &server, "gpt-5");

    let (head, body) = split_request(&only_request(&server));
    assert!(head.starts_with("POST /chat/completions"), "{head}");
    let lowered = head.to_ascii_lowercase();
    assert!(
        lowered.contains("authorization: bearer test-key"),
        "the credential travels as a bearer token: {head}"
    );
    assert!(lowered.contains("accept: text/event-stream"), "{head}");
    assert_eq!(body["model"], serde_json::json!("gpt-5"));
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["reasoning_effort"], serde_json::json!("medium"));
    // OpenAI's reasoning models name the ceiling `max_completion_tokens`.
    assert_eq!(body["max_completion_tokens"], serde_json::json!(128_000));
    assert!(body.get("max_tokens").is_none(), "{body}");

    // The head is announced before the body, which is the split a latency reading needs.
    assert!(matches!(events.first(), Some(LlmEvent::ResponseHead)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::ReasoningDelta(text) if text == "thinking"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "Hello"))
    );

    // The fragments assemble into one call, through the same assembler the loop uses.
    let mut assembler = nanus_ports::ToolCallAssembler::new();
    for event in &events {
        assert!(assembler.apply_event(event).is_ok());
    }
    let calls = assembler.finish().expect("the call assembles");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls
            .first()
            .map(|call| call.arguments.get("path").cloned()),
        Some(Some(serde_json::json!("a.txt")))
    );

    // The accounting survives the vendor's spelling of the cache counters.
    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .expect("usage is reported");
    assert_eq!(usage.prompt_tokens, 11);
    assert_eq!(usage.cache_hit_tokens, 4);
    assert_eq!(usage.cache_miss_tokens, 7, "derived from the prompt count");
    assert_eq!(usage.reasoning_tokens, 3);
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Finished {
            reason: nanus_ports::FinishReason::ToolCalls
        })
    ));
}

/// The other vendor's control is on the wire instead, which is the one thing that differs
/// between them at the request level — and the ceiling is the vendor's own.
#[test]
fn the_zai_vendor_sends_its_thinking_switch_and_effort() {
    let server = spawn_server(200, "data: [DONE]\n\n");
    let _ = collect(nanus_adapter_openai::Vendor::Zai, &server, "glm-4.5");
    let (head, body) = split_request(&only_request(&server));
    assert!(head.starts_with("POST /chat/completions"), "{head}");
    // The switch is always on: GLM-5.3 refuses a disabled one, so "off" is the `none` effort.
    assert_eq!(body["thinking"]["type"], serde_json::json!("enabled"));
    assert_eq!(body["reasoning_effort"], serde_json::json!("medium"));
    assert_eq!(body["max_tokens"], serde_json::json!(98_304));
}

/// A refusal arrives as the provider's own words rather than as a bare status, which is
/// what a reader needs to fix it.
#[test]
fn a_provider_error_is_reported_with_its_message() {
    let server = spawn_server(
        429,
        r#"{"error":{"message":"rate limit reached","type":"rate_limit"}}"#,
    );
    let events = collect(nanus_adapter_openai::Vendor::OpenAi, &server, "gpt-5");
    // The head arrived, because the server did answer — a refusal is an answer. What the
    // caller must also get is the provider's reason, as the terminal event.
    assert!(matches!(events.first(), Some(LlmEvent::ResponseHead)));
    let event = events.last();
    assert!(
        matches!(&event, Some(LlmEvent::Error(message))
            if message.contains("429") && message.contains("rate limit reached")),
        "{event:?}"
    );
    // Exactly one error, and nothing after it: a stream must not end silently.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LlmEvent::Error(_)))
            .count(),
        1,
        "{events:?}"
    );
}

/// The subscription path: a Responses request over a real socket, streaming named events rather
/// than `choices`, with the `ChatGPT` account named in its own header.
///
/// The gap this closes is the same one the chat tests close: that the adapter really sends the
/// Responses body to `/responses` with the account header, and that a real `text/event-stream` of
/// named events decodes into the events the loop expects — text, an assembled tool call, usage, and
/// an ending.
#[test]
fn the_subscription_speaks_the_responses_api() {
    let events = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\\\"file_path\\\":\\\"a.rs\\\"}\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":3}}}}\n\n",
        "data: [DONE]\n\n",
    );
    let server = spawn_server(200, events);

    let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-5.3-codex",
        "access-token",
        &server.base_url,
    );
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    config.set_account_id("acct-1");
    let llm = nanus_adapter_openai::OpenAiLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(
        "gpt-5.3-codex",
        vec![Message::system("be terse"), Message::user("hi")],
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let collected = runtime.block_on(async {
        let mut events = Vec::new();
        let mut stream = llm.stream_chat(request);
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    });

    let (head, body) = split_request(&only_request(&server));
    assert!(head.starts_with("POST /responses"), "{head}");
    assert!(
        head.to_lowercase().contains("chatgpt-account-id: acct-1"),
        "the account travels in its own header: {head}"
    );
    assert!(body.get("messages").is_none(), "not the chat shape: {body}");
    assert!(body.get("input").is_some(), "the items shape: {body}");
    assert!(body.get("instructions").is_some(), "the prompt is lifted");

    assert!(
        collected
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "hello")),
        "{collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, LlmEvent::ToolCallDelta { .. })),
        "the call is assembled: {collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, LlmEvent::Usage(_))),
        "usage is decoded: {collected:?}"
    );
    assert!(
        matches!(collected.last(), Some(LlmEvent::Finished { .. })),
        "the stream ends with a reason: {collected:?}"
    );
}
