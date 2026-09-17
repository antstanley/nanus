//! The wire path over a real socket: HTTP framing, SSE decoding, tool-call reassembly.
//!
//! Everything between `DeepSeekLlm::stream_chat` and a finished tool result is exercised
//! here against a **real TCP server**, not a mock stream. The gap this closes is the one
//! a unit test on `SseDecoder` cannot: that the chunks arrive as the network delivers
//! them, split mid-frame and mid-multibyte-character, and that the assembled tool call
//! then reaches the real `glob` tool and produces a second step.
//!
//! What it deliberately does **not** prove is that the live API's framing matches this
//! replay. A live text run already confirmed the request shape, TLS, auth, the streaming
//! transport and the reasoning passback rule; the only thing left unverified is a live
//! *tool call*, and this is the closest a hermetic test can get to it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use nanus_domain::{AgentConfig, Session, SessionId};
// The adapter's methods come from the port trait, so a test that drives it directly needs the
// trait in scope rather than only the type.
use nanus_ports::{LlmPort as _, SandboxPolicy};

/// One scripted HTTP response, as the server should write it.
#[derive(Clone)]
struct Response {
    body: String,
}

/// Serves `responses` in order, one per request, until the script is exhausted.
///
/// A hand-written HTTP/1.1 server rather than a framework: the harness's dependency set
/// is part of what is being kept small, and the framing under test is a dozen lines.
///
/// Two details matter for the test to be about the *client* rather than about this
/// server. It answers on the connection the request arrived on and keeps reading that
/// connection until the client closes it, because `reqwest` pools connections and a
/// server that accepted only one request per socket would silently swallow the second
/// step. And it stops as soon as the script is exhausted, so the harness cannot hang
/// waiting for a response that will never come.
fn spawn_server(responses: Vec<Response>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);

    std::thread::spawn(move || {
        let mut remaining = responses.into_iter();
        while let Ok((stream, _)) = listener.accept() {
            let Some(response) = remaining.next() else {
                // The script is done. The connection is dropped, which tells the client
                // there is nothing more.
                return;
            };
            let Ok(stream) = serve(stream, &response, &seen) else {
                return;
            };
            drop(stream);
        }
    });

    (format!("http://{address}"), requests)
}

/// Reads one request from `stream`, answers it, and returns the stream for reuse.
fn serve(
    mut stream: std::net::TcpStream,
    response: &Response,
    seen: &Arc<std::sync::Mutex<Vec<String>>>,
) -> std::io::Result<std::net::TcpStream> {
    let _ignored = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
    let request = read_request(&mut stream);
    seen.lock()
        .expect("the request log is not poisoned")
        .push(request);

    // `connection: close` after each response keeps the client from racing ahead with a
    // pooled connection this loop has already moved past.
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         content-type: text/event-stream\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()?;
    Ok(stream)
}

/// Reads one HTTP request, returning its raw text.
///
/// The body is read until `content-length` bytes have arrived, which is what keeps a
/// large request (the tool catalogue makes it several kilobytes) from being truncated
/// and turning the test into a framing assertion about this helper.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut expected: Option<usize> = None;
    loop {
        // A read of zero is the client closing, and an error is the read timeout firing
        // because the client is waiting for a response. Either way the request is
        // complete as far as it is going to get.
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
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let _ = head;
                if body.len() >= expected {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Builds one SSE frame carrying `payload`.
fn frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

/// Builds the SSE body that streams a tool call in fragments.
fn tool_call_stream(tool: &str, arguments: &str) -> String {
    let mut body = String::new();
    // An opening frame that only announces the call, with no arguments yet.
    body.push_str(&frame(&format!(
        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"call-1","type":"function","function":{{"name":"{tool}","arguments":""}}}}]}}}}]}}"#
    )));
    // The arguments split across two frames, mid-JSON, as a real stream does. A shift
    // rather than a division: the halves must land on a `char` boundary, and an even
    // split of a byte length is what a real fragment boundary looks like.
    let (first, second) = arguments.split_at(arguments.len().wrapping_shr(1));
    body.push_str(&frame(&format!(
        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{}}}}}]}}}}]}}"#,
        serde_json::to_string(first).expect("a JSON string")
    )));
    body.push_str(&frame(&format!(
        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{}}}}}]}}}}]}}"#,
        serde_json::to_string(second).expect("a JSON string")
    )));
    body.push_str(&frame(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// Builds the SSE body for a plain answer, with reasoning before it.
fn answer_stream(text: &str) -> String {
    let mut body = String::new();
    body.push_str(&frame(
        r#"{"choices":[{"delta":{"reasoning_content":"considering"}}]}"#,
    ));
    body.push_str(&frame(&format!(
        r#"{{"choices":[{{"delta":{{"content":{}}}}}]}}"#,
        serde_json::to_string(text).expect("a JSON string")
    )));
    body.push_str(&frame(
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":21,"completion_tokens":5,"prompt_cache_hit_tokens":10,"prompt_cache_miss_tokens":11}}"#,
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// Builds a runner pointed at `base_url` over a temporary workspace.
fn runner(
    base_url: &str,
    workspace: &std::path::Path,
) -> (nanus_bundle::AgentRunner, nanus_ports::ShellHandle) {
    let fs = nanus_adapter_local::LocalFs::new(workspace)
        .expect("a temporary workspace is readable")
        .handle();
    let shell = nanus_adapter_local::LocalShell::new(SandboxPolicy::new(
        nanus_domain::SandboxMode::WorkspaceWrite,
        workspace,
    ))
    .handle();
    let registry = nanus_bundle::build_toolset(&fs, &shell).expect("the toolset builds");

    let adapter = nanus_adapter_deepseek::DeepSeekConfig::with_base_url(
        nanus_adapter_deepseek::MODEL_FLASH,
        "test-key",
        base_url,
    );
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(adapter).expect("the adapter builds");
    let port: Box<dyn nanus_ports::LlmPort> = Box::new(llm);

    // Full access, so the real tools run without an answerer: this test is about the wire,
    // not the approval gate.
    let config = AgentConfig::new(8, 4, nanus_adapter_deepseek::MODEL_FLASH, 16_384)
        .expect("a valid agent configuration")
        .with_sandbox(nanus_domain::SandboxMode::DangerFullAccess);
    let runner =
        nanus_bundle::AgentRunner::new(Rc::new(port), Rc::new(registry), "you are a test", config)
            .expect("the runner builds");
    (runner, shell)
}

#[tokio::test]
async fn a_streamed_tool_call_runs_the_real_tool_and_produces_a_second_step() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::write(root.join("alpha.rs"), "// a crate root\n").expect("seed a file");
    std::fs::write(root.join("beta.rs"), "// another\n").expect("seed a file");

    // The arguments arrive split across frames, so the reassembly is genuinely tested.
    let responses = vec![
        Response {
            body: tool_call_stream("glob", r#"{"pattern":"**/*.rs"}"#),
        },
        Response {
            body: answer_stream("There are two Rust files."),
        },
    ];
    let (base_url, requests) = spawn_server(responses);
    let (runner, shell) = runner(&base_url, root);

    let mut session = Session::new(SessionId::new("wire"), 0, "/tmp");
    let outcome = runner
        .run_turn(
            &mut session,
            "list the rust files",
            &mut nanus_bundle::Silent,
            None,
        )
        .await;
    let outcome = outcome.expect("the turn completes over a real socket");
    assert_eq!(outcome.steps, 2, "a tool call owes a second step");
    assert_eq!(outcome.answer, "There are two Rust files.");
    assert!(
        outcome.is_success(),
        "finished cleanly: {:?}",
        outcome.reason
    );

    // The request carried all seven schemas, so the server saw a real tool catalogue.
    // The guard is scoped so it cannot be held across the later await.
    let observed = {
        let seen = requests.lock().expect("the request log is not poisoned");
        seen.clone()
    };
    assert_eq!(observed.len(), 2, "one request per step");
    let first = &observed[0];
    for tool in [
        "glob",
        "grep",
        "read",
        "write",
        "edit",
        "read_image",
        "bash",
    ] {
        assert!(first.contains(tool), "the request advertises {tool}");
    }
    // And the second request carried the tool result back, which is what makes the
    // round trip a round trip.
    assert!(
        observed[1].contains("alpha.rs"),
        "the tool result reached the model"
    );

    // Tokens from both responses are summed, proving the usage path survived decoding.
    assert_eq!(outcome.usage.prompt_tokens, 32);
    assert_eq!(outcome.usage.completion_tokens, 12);

    let reaped = shell.kill_all().await;
    assert!(reaped.is_ok());
}

#[tokio::test]
async fn a_text_answer_over_a_real_socket_records_reasoning_and_usage() {
    let dir = tempfile::tempdir().expect("temp dir");
    let responses = vec![Response {
        body: answer_stream("Hello there."),
    }];
    let (base_url, _requests) = spawn_server(responses);
    let (runner, _shell) = runner(&base_url, dir.path());

    let mut session = Session::new(SessionId::new("wire-text"), 0, "/tmp");
    let outcome = runner
        .run_turn(&mut session, "say hi", &mut nanus_bundle::Silent, None)
        .await
        .expect("the turn completes");

    assert_eq!(outcome.answer, "Hello there.");
    assert_eq!(outcome.steps, 1);
    // Reasoning arrived before the text and must be in the log, because the API requires
    // it to be replayed when tools are present.
    let has_reasoning = session.log().events().iter().any(|event| {
        matches!(
            event,
            nanus_domain::SessionEvent::AssistantMessage { reasoning: Some(text), .. }
                if text == "considering"
        )
    });
    assert!(has_reasoning, "the reasoning trace is recorded");
    // Usage was reported this time, so it is recorded as present rather than absent.
    assert_eq!(outcome.usage.prompt_tokens, 21);
    assert_eq!(outcome.usage.cache_hit_tokens, 10);
}

#[tokio::test]
async fn a_frame_whose_last_character_arrives_later_still_decodes() {
    // The case a decoder that converted each chunk to text on arrival would corrupt: a
    // frame whose final character is multi-byte, delivered in a chunk that ends inside
    // that character. The decoder must hold bytes until the frame's newline arrives
    // rather than decoding what it has.
    //
    // The split is made at the character's own byte offset, which `str` guarantees is a
    // boundary; the *byte stream* the harness reads is still split mid-character, which
    // is the property under test.
    let text = "caf\u{e9} time";
    let body = answer_stream(text);
    // `serde_json` writes the character literally, so the escape is a real `é`.
    let character = body
        .find('\u{e9}')
        .unwrap_or_else(|| panic!("the body carries the character: {body:?}"));
    // Precondition: the character really is multi-byte, or the test proves nothing.
    assert_eq!(
        '\u{e9}'.len_utf8(),
        2,
        "the character is two bytes in UTF-8"
    );
    let (first, second) = (
        body.get(..character).expect("a byte prefix").to_owned(),
        body.get(character..).expect("a byte suffix").to_owned(),
    );
    // The first chunk ends immediately before the character, so the frame it belongs to
    // has not finished arriving.
    assert!(
        first.ends_with("caf"),
        "the first chunk ends before the character: {first:?}"
    );
    assert!(
        second.starts_with('\u{e9}'),
        "the second chunk starts with it"
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let (base_url, _requests) =
        spawn_server(vec![Response { body: first }, Response { body: second }]);
    let (runner, _shell) = runner(&base_url, dir.path());

    let mut session = Session::new(SessionId::new("wire-split"), 0, "/tmp");
    let outcome = runner
        .run_turn(&mut session, "say it", &mut nanus_bundle::Silent, None)
        .await;

    // The first delivery stops inside a frame, so the response really is truncated. The
    // adapter must say so — a client that silently accepted a half-frame would hand the
    // model a mangled answer — and it must not invent a replacement character while
    // doing it.
    let error = outcome.expect_err("a response truncated mid-frame is reported");
    let message = error.to_string();
    assert!(
        message.contains("not JSON"),
        "the failure names the cause: {message}"
    );
    assert!(
        message.contains("caf"),
        "the failure quotes the fragment it could not decode: {message}"
    );
    let corrupted = session.log().events().iter().any(|event| {
        matches!(
            event,
            nanus_domain::SessionEvent::AssistantMessage { text: Some(text), .. }
                if text.contains('\u{fffd}')
        )
    });
    assert!(!corrupted, "no replacement character was invented");
}

/// The response head is announced before any of the body.
///
/// This is the whole worth of the event: it is the boundary a request's wait is split at, so a
/// head announced after the first token would put that token's wait on the wrong side of the
/// split — and the split would then be measuring the wrong interval while looking correct. The
/// assertion is on the order, not on the count, because the count would be satisfied by an
/// announcement that arrived too late to divide anything.
#[tokio::test]
async fn the_response_head_is_announced_before_the_body() {
    // No workspace: this drives the adapter's own stream rather than a turn, and nothing here
    // reaches a tool.
    let (base_url, _requests) = spawn_server(vec![Response {
        body: answer_stream("hello"),
    }]);
    let adapter = nanus_adapter_deepseek::DeepSeekConfig::with_base_url(
        nanus_adapter_deepseek::MODEL_FLASH,
        "test-key",
        &base_url,
    );
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(adapter).expect("the adapter builds");
    let request = nanus_ports::ChatRequest::new(
        nanus_adapter_deepseek::MODEL_FLASH,
        vec![nanus_domain::Message::user("hi")],
    );

    let events: Vec<nanus_ports::LlmEvent> = llm.stream_chat(request).collect().await;

    // The other direction first: this body does carry generated content, so an announcement ahead
    // of nothing would satisfy an order assertion vacuously.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, nanus_ports::LlmEvent::TextDelta(_))),
        "the body carried text to be announced before: {events:?}"
    );
    assert_eq!(
        events.first(),
        Some(&nanus_ports::LlmEvent::ResponseHead),
        "the head comes before everything the body said: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, nanus_ports::LlmEvent::ResponseHead))
            .count(),
        1,
        "and is announced once, because a second announcement would move the boundary"
    );
}
