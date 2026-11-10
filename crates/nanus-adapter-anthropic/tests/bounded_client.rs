//! The two bounds on a request that are the HTTP client's: it follows no redirect, and it does
//! not wait for ever.
//!
//! Both are pinned against real sockets, because both are decided by the client the adapter
//! builds rather than by anything the adapter's own code does afterwards — a mock could not see
//! a request leave for another host, or a wait that never ends.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use nanus_domain::Message;
use nanus_ports::{ChatRequest, HttpTimeouts, LlmEvent, LlmPort as _};

/// Serves one connection: the head, `first`, a pause of `silence`, then `rest`, and closes.
///
/// The body is declared at its whole length, so a client that stops waiting during the pause
/// has stopped on its own deadline rather than at an end of body.
fn a_server_that_pauses(first: &'static str, silence: Duration, rest: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let head = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             content-length: {}\r\n\
             connection: close\r\n\r\n",
            first.len().saturating_add(rest.len())
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(first.as_bytes());
        let _ = stream.flush();
        std::thread::sleep(silence);
        let _ = stream.write_all(rest.as_bytes());
        let _ = stream.flush();
    });
    format!("http://{address}")
}

/// Accepts connections for ever and counts them, answering nothing.
///
/// What a redirect would be followed to: a connection here is the request, and the credential
/// in its headers, having left the endpoint it was built for.
fn a_host_that_counts_its_visitors() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    let visits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&visits);
    std::thread::spawn(move || {
        while listener.accept().is_ok() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    (format!("http://{address}"), visits)
}

/// Serves one connection with a `307` whose `location` is `target`.
fn a_server_that_redirects_to(target: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    let head = format!(
        "HTTP/1.1 307 Temporary Redirect\r\n\
         location: {target}\r\n\
         content-length: 0\r\n\
         connection: close\r\n\r\n"
    );
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
    });
    format!("http://{address}")
}

/// Runs `events` to completion on a runtime of its own.
fn run(events: impl Future<Output = Vec<LlmEvent>>) -> Vec<LlmEvent> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(events)
}

/// The text the events carried, joined.
fn text_of(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The first half of an answer, and the half that finishes it.
const FIRST_HALF: &str = concat!(
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"one\"}}\n\n",
);
const SECOND_HALF: &str = concat!(
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\" two\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The model every request here names.
const MODEL: &str = "claude-sonnet-4-20250514";

/// Drives one request, bounded by `timeouts`, to the end of its stream.
fn every_event(base_url: &str, timeouts: HttpTimeouts) -> Vec<LlmEvent> {
    let mut config =
        nanus_adapter_anthropic::AnthropicConfig::with_base_url(MODEL, "test-key", base_url);
    config.set_timeouts(timeouts);
    let llm = nanus_adapter_anthropic::AnthropicLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(MODEL, vec![Message::user("hi")]);
    run(async move {
        let mut events = Vec::new();
        let mut stream = llm.stream_chat(request);
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

/// A provider that goes silent part-way through a response used to hold the turn for ever: no
/// byte, no error, and nothing but an interrupt to end it. The silence is bounded now, and the
/// failure says that a wait ran out.
#[test]
fn a_response_that_goes_silent_is_given_up_on() {
    let base_url = a_server_that_pauses(FIRST_HALF, Duration::from_secs(5), SECOND_HALF);
    let timeouts = HttpTimeouts::new(Duration::from_secs(5), Duration::from_millis(300));
    let started = std::time::Instant::now();
    let events = every_event(&base_url, timeouts);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the adapter stopped waiting before the server spoke again"
    );
    let Some(LlmEvent::Error(message)) = events.last() else {
        panic!("the silence is reported: {events:?}");
    };
    assert!(message.contains("timed out"), "{message}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. })),
        "{events:?}"
    );
}

/// The other direction: the bound is on a silence, not on the response. A pause shorter than
/// it — a model thinking between two frames — changes nothing.
#[test]
fn a_pause_shorter_than_the_bound_is_only_a_pause() {
    let base_url = a_server_that_pauses(FIRST_HALF, Duration::from_millis(300), SECOND_HALF);
    let timeouts = HttpTimeouts::new(Duration::from_secs(5), Duration::from_secs(5));
    let events = every_event(&base_url, timeouts);
    assert_eq!(text_of(&events), "one two");
    assert!(
        matches!(events.last(), Some(LlmEvent::Finished { .. })),
        "{events:?}"
    );
}

/// A `307` asks the client to post the same request somewhere else, and this
/// adapter's key travels in `x-api-key`, a header an HTTP client does not strip on the way.
/// It is not followed: the request stays with the endpoint it was built for, and the redirect
/// is reported as the status it is.
#[test]
fn a_redirect_is_reported_rather_than_followed() {
    let (elsewhere, visits) = a_host_that_counts_its_visitors();
    let base_url = a_server_that_redirects_to(&format!("{elsewhere}/v1/messages"));
    let events = every_event(&base_url, HttpTimeouts::default());
    let Some(LlmEvent::Error(message)) = events.last() else {
        panic!("the redirect is reported: {events:?}");
    };
    assert!(message.contains("307"), "{message}");
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "nothing — no body and no credential — was sent to the redirect's target"
    );
}
