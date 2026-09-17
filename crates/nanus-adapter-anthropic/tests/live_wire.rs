//! The wire path over a real socket: HTTP framing, event-typed SSE decoding, tool use.
//!
//! Everything between `AnthropicLlm::stream_chat` and the events the agent loop consumes
//! is exercised here against a **real TCP server**. What this closes that a unit test on
//! the accumulator cannot: that the request really carries the Messages shape — a
//! top-level `system`, a tool result as a *user* turn, `x-api-key` and the version header
//! — and that a stream which ends at `message_stop` rather than at a `[DONE]` sentinel is
//! decoded to the end rather than left hanging.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName};
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

/// Builds one event-typed SSE frame.
///
/// Both the `event:` name and the payload's own `type` are written, because that is what
/// the API sends and what the decoder must tolerate: the framer ignores the name and the
/// accumulator reads the field.
fn event(kind: &str, payload: &str) -> String {
    format!("event: {kind}\ndata: {payload}\n\n")
}

/// Builds the stream of a tool-using turn, ending at `message_stop`.
fn tool_use_stream() -> String {
    let mut body = String::new();
    body.push_str(&event(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}"#,
    ));
    body.push_str(&event(
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ));
    body.push_str(&event(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
    ));
    body.push_str(&event(
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ));
    body.push_str(&event(
        "content_block_start",
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
    ));
    body.push_str(&event(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
    ));
    body.push_str(&event(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
    ));
    body.push_str(&event(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
    ));
    body.push_str(&event("message_stop", r#"{"type":"message_stop"}"#));
    body
}

/// Returns a tool name for the fixture.
fn tool_name(raw: &str) -> ToolName {
    ToolName::new(raw).expect("a valid test tool name")
}

/// The conversation sent to the API: a system turn, a tool call, and its result.
fn conversation() -> Vec<Message> {
    vec![
        Message::system("you are nanus"),
        Message::user("hi"),
        Message::assistant(
            None,
            None,
            vec![ToolCall::new(
                ToolCallId::new("toolu_1"),
                tool_name("read"),
                serde_json::json!({ "path": "a.txt" }),
            )],
        ),
        Message::tool(ToolCallId::new("toolu_1"), "contents", false),
    ]
}

/// Runs the conversation against `server`, returning every event.
fn collect(server: &Server) -> Vec<LlmEvent> {
    let config = nanus_adapter_anthropic::AnthropicConfig::with_base_url(
        "claude-sonnet-4-20250514",
        "test-key",
        &server.base_url,
    );
    let llm = nanus_adapter_anthropic::AnthropicLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new("claude-sonnet-4-20250514", conversation());
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

/// Returns the one request the server was sent, split into head and decoded body.
fn only_request(server: &Server) -> (String, Value) {
    let captured = server
        .requests
        .lock()
        .expect("the request log")
        .first()
        .cloned()
        .expect("a request arrived");
    let (head, body) = captured
        .split_once("\r\n\r\n")
        .expect("a request has a body");
    let parsed: Value = serde_json::from_str(body).expect("the body is JSON");
    (head.to_owned(), parsed)
}

/// The request shape the Messages API requires, asserted on the bytes that were sent.
#[test]
fn the_request_carries_the_messages_shape() {
    let server = spawn_server(200, &tool_use_stream());
    let _ = collect(&server);
    let (head, body) = only_request(&server);
    assert!(head.starts_with("POST /messages"), "{head}");
    let lowered = head.to_ascii_lowercase();
    assert!(lowered.contains("x-api-key: test-key"), "{head}");
    // The version is declared on every request: there is no "latest".
    assert!(lowered.contains("anthropic-version: 2023-06-01"), "{head}");
    assert!(lowered.contains("accept: text/event-stream"), "{head}");

    // The system turn is lifted to the top level rather than being a message.
    assert_eq!(body["system"], serde_json::json!("you are nanus"));
    assert_eq!(body["max_tokens"], serde_json::json!(64_000));
    assert_eq!(body["stream"], serde_json::json!(true));
    let turns = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert_eq!(turns[0]["role"], serde_json::json!("user"));
    // A tool call's arguments are an object here, not a JSON string.
    assert_eq!(
        turns[1]["content"][0]["type"],
        serde_json::json!("tool_use")
    );
    assert_eq!(
        turns[1]["content"][0]["input"]["path"],
        serde_json::json!("a.txt")
    );
    // A tool result is a *user* turn carrying a tool_result block.
    assert_eq!(turns[2]["role"], serde_json::json!("user"));
    assert_eq!(
        turns[2]["content"][0]["type"],
        serde_json::json!("tool_result")
    );
    assert_eq!(
        turns[2]["content"][0]["tool_use_id"],
        serde_json::json!("toolu_1")
    );
}

/// The stream ends at `message_stop`, which is not a sentinel: the events after it must
/// still be emitted, or a tool-using turn would hang waiting for a `[DONE]` that this API
/// never sends.
#[test]
fn a_streamed_tool_use_becomes_events_and_ends_at_message_stop() {
    let server = spawn_server(200, &tool_use_stream());
    let events = collect(&server);

    assert!(matches!(events.first(), Some(LlmEvent::ResponseHead)));
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
        calls.first().map(|call| call.id.as_str().to_owned()),
        Some(String::from("toolu_1"))
    );
    assert_eq!(
        calls
            .first()
            .map(|call| call.arguments.get("path").cloned()),
        Some(Some(serde_json::json!("a.txt")))
    );

    // The prompt is the whole prompt, and the two counters partition it.
    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .expect("usage is reported");
    assert_eq!(usage.prompt_tokens, 20);
    assert_eq!(usage.cache_hit_tokens, 4);
    assert_eq!(usage.cache_miss_tokens, 16);
    assert_eq!(
        usage
            .cache_hit_tokens
            .saturating_add(usage.cache_miss_tokens),
        usage.prompt_tokens
    );
    assert_eq!(usage.completion_tokens, 7);

    assert!(matches!(
        events.last(),
        Some(LlmEvent::Finished {
            reason: nanus_ports::FinishReason::ToolCalls
        })
    ));
}

/// A refusal arrives as the provider's own words, and 529 — "overloaded" — is its own
/// status rather than a generic server fault.
#[test]
fn a_provider_error_is_reported_with_its_message() {
    let server = spawn_server(
        529,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    );
    let events = collect(&server);
    let event = events.last();
    assert!(
        matches!(&event, Some(LlmEvent::Error(message))
            if message.contains("529") && message.contains("Overloaded")),
        "{event:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LlmEvent::Error(_)))
            .count(),
        1,
        "{events:?}"
    );
}
