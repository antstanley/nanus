//! A transport failure names the endpoint the request was actually sent to.
//!
//! The host in a transport error is read by somebody deciding what to do next, and the whole
//! point of a custom `base_url` is that the request went somewhere else: telling a user that
//! `api.deepseek.com` was unreachable when their request went to a local proxy sends them to
//! debug the wrong machine.
//!
//! The same sockets pin what a body that ends *cleanly* means: a response that named its finish
//! reason is over, and one that never did still finishes but does not claim a `Stop`.
//!
//! Both paths that can report one are covered, against a real socket rather than a mock,
//! because the two fail in different places: the request that never gets a response, and the
//! response body that stops arriving part-way through.

// A panic in a test *is* the assertion, and a fixture with no sane default has nowhere else
// to put the failure. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm, MODEL_FLASH};
use nanus_domain::Message;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort as _};

/// Serves one connection with a body shorter than the length it declares.
///
/// The client is told to expect more than it will receive, so its byte stream ends in an
/// error rather than at a clean end of body. Returns the base URL to point an adapter at.
fn a_server_that_stops_mid_body() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // One read is enough to consume the request head and unblock the write below.
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let head = "HTTP/1.1 200 OK\r\n\
                    content-type: text/event-stream\r\n\
                    content-length: 4096\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(b"data: {\"choices\":[{\"delta\"");
        let _ = stream.flush();
        // Dropping the stream closes it with most of the declared body still owed.
    });
    format!("http://{address}")
}

/// Returns a base URL that nothing is listening on.
///
/// The port is bound and released rather than hard-coded, so the test cannot collide with a
/// real service on a developer's machine.
fn an_address_nothing_listens_on() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);
    format!("http://{address}")
}

/// Drives one request to the end of its stream and returns the error it reported.
///
/// The stream is read to exhaustion rather than stopped at the first event, because "the
/// failure is reported" and "nothing follows it" are both part of the contract.
async fn the_only_failure(base_url: &str) -> String {
    let config = DeepSeekConfig::with_base_url(MODEL_FLASH, "test-key", base_url);
    let llm = DeepSeekLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(MODEL_FLASH, vec![Message::user("hi")]);
    let mut stream = llm.stream_chat(request);

    let mut failure = None;
    while let Some(event) = stream.next().await {
        if let LlmEvent::Error(message) = event {
            assert!(
                failure.is_none(),
                "one terminal failure, not two: {message}"
            );
            failure = Some(message);
        }
    }
    failure.expect("the stream reports why it could not be read")
}

/// Asserts that `message` names `base_url` and not the default host.
///
/// Both directions matter: a host that is missing is the defect, and a host that is the
/// *default* is the defect with a plausible-looking answer.
fn assert_names(message: &str, base_url: &str) {
    let host = base_url.trim_start_matches("http://");
    assert!(
        message.contains(host),
        "the failure names the configured host {host}: {message}"
    );
    assert!(
        !message.contains("api.deepseek.com"),
        "the failure does not name the default host: {message}"
    );
}

#[tokio::test]
async fn a_request_that_cannot_connect_names_the_configured_host() {
    let base_url = an_address_nothing_listens_on();
    let message = the_only_failure(&base_url).await;
    assert_names(&message, &base_url);
}

#[tokio::test]
async fn a_body_that_stops_early_names_the_configured_host() {
    let base_url = a_server_that_stops_mid_body();
    let message = the_only_failure(&base_url).await;
    assert_names(&message, &base_url);
}

/// Serves one connection with exactly `body`, declared at its true length, and closes.
///
/// A clean end of body: the client's byte stream finishes without an error, so what the adapter
/// makes of it depends only on what the frames said.
fn a_server_that_answers(body: &'static str) -> String {
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
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    });
    format!("http://{address}")
}

/// Drives one request to the end of its stream and returns every event.
async fn every_event(base_url: &str) -> Vec<LlmEvent> {
    every_event_within(base_url, nanus_ports::HttpTimeouts::default()).await
}

/// Drives one request, bounded by `timeouts`, to the end of its stream.
async fn every_event_within(base_url: &str, timeouts: nanus_ports::HttpTimeouts) -> Vec<LlmEvent> {
    let mut config = DeepSeekConfig::with_base_url(MODEL_FLASH, "test-key", base_url);
    config.set_timeouts(timeouts);
    let llm = DeepSeekLlm::new(config).expect("the adapter builds");
    let request = ChatRequest::new(MODEL_FLASH, vec![Message::user("hi")]);
    let mut stream = llm.stream_chat(request);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// A body that ends cleanly having named no finish reason and sent no sentinel simply stopped
/// arriving — a gateway that gave up on its upstream, say. The ordinary path still finishes
/// such a stream, as it always has, but it used to report a `Stop` the provider never sent,
/// which made half a reply indistinguishable from a whole one. The reason now says so.
#[tokio::test]
async fn a_body_that_ends_with_no_finish_reason_does_not_claim_a_stop() {
    let base_url =
        a_server_that_answers("data: {\"choices\":[{\"delta\":{\"content\":\"half an\"}}]}\n\n");
    let events = every_event(&base_url).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "half an")),
        "what arrived is still shown: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(LlmEvent::Finished {
            reason: nanus_ports::FinishReason::Unknown(reason)
        }) if reason == nanus_ports::UNTERMINATED),
        "the stream still finishes, and says nobody ended it: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Error(_))),
        "a closed socket is not an error on the ordinary path: {events:?}"
    );
}

/// The other direction, and the behaviour that must not move: a server need not send the
/// sentinel, so a body that named its finish reason and then simply closed is a whole answer.
#[tokio::test]
async fn a_body_that_named_its_finish_reason_needs_no_sentinel() {
    let base_url = a_server_that_answers(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"whole\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ));
    let events = every_event(&base_url).await;
    assert!(
        matches!(
            events.last(),
            Some(LlmEvent::Finished {
                reason: nanus_ports::FinishReason::Stop
            })
        ),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Error(_))),
        "{events:?}"
    );
}

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

/// The first half of an answer, and the half that finishes it.
const FIRST_HALF: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n";
const SECOND_HALF: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\" two\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// A provider that goes silent part-way through a response used to hold the turn for ever: no
/// byte, no error, and nothing but an interrupt to end it. The silence is bounded now, and the
/// failure says that a wait ran out and where.
#[tokio::test]
async fn a_response_that_goes_silent_is_given_up_on() {
    let base_url = a_server_that_pauses(FIRST_HALF, Duration::from_secs(5), SECOND_HALF);
    let timeouts =
        nanus_ports::HttpTimeouts::new(Duration::from_secs(5), Duration::from_millis(300));
    let started = std::time::Instant::now();
    let events = every_event_within(&base_url, timeouts).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the adapter stopped waiting before the server spoke again"
    );
    let Some(LlmEvent::Error(message)) = events.last() else {
        panic!("the silence is reported: {events:?}");
    };
    assert!(message.contains("timed out"), "{message}");
    assert_names(message, &base_url);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. })),
        "{events:?}"
    );
}

/// The other direction: the bound is on a silence, not on the response. A pause shorter than
/// it — a model thinking between two frames — changes nothing.
#[tokio::test]
async fn a_pause_shorter_than_the_bound_is_only_a_pause() {
    let base_url = a_server_that_pauses(FIRST_HALF, Duration::from_millis(300), SECOND_HALF);
    let timeouts = nanus_ports::HttpTimeouts::new(Duration::from_secs(5), Duration::from_secs(5));
    let events = every_event_within(&base_url, timeouts).await;
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "one two");
    assert!(
        matches!(events.last(), Some(LlmEvent::Finished { .. })),
        "{events:?}"
    );
}

/// Accepts connections for ever and counts them, answering nothing.
///
/// What a redirect would be followed to: a connection here is the request having left the
/// endpoint it was built for.
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

/// Serves one connection with `status` and a `location` header naming `target`.
fn a_server_that_answers_with(status: u16, target: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the bound address");
    let head = format!(
        "HTTP/1.1 {status} Elsewhere\r\n\
         location: {target}/chat/completions\r\n\
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

/// A `307` asks the client to post the same body — the whole conversation — somewhere else. It
/// is not followed: the request stays with the endpoint it was built for, and the redirect is
/// reported as the status it is.
#[tokio::test]
async fn a_redirect_is_reported_rather_than_followed() {
    let (elsewhere, visits) = a_host_that_counts_its_visitors();
    let base_url = a_server_that_answers_with(307, &elsewhere);
    let events = every_event(&base_url).await;
    let Some(LlmEvent::Error(message)) = events.last() else {
        panic!("the redirect is reported: {events:?}");
    };
    assert!(message.contains("307"), "{message}");
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "nothing was sent to the redirect's target"
    );
}
