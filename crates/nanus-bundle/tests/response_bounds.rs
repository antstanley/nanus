//! Opt-in response policy exercised through real HTTP for every supported API grammar.
#![cfg(feature = "stock-compose")]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
use futures::StreamExt as _;
use nanus_domain::Message;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream, ResponseLimits};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Grammar {
    Anthropic,
    DeepSeek,
    Chat,
    Responses,
}
const GRAMMARS: [Grammar; 4] = [
    Grammar::Anthropic,
    Grammar::DeepSeek,
    Grammar::Chat,
    Grammar::Responses,
];
fn limits() -> ResponseLimits {
    ResponseLimits::new(1024, 512, 8192, 32, 2, 256).expect("limits")
}
impl Grammar {
    fn stream(self, base: &str, limits: Option<ResponseLimits>) -> LlmStream {
        let request = ChatRequest::new(self.model(), vec![Message::user("hi")]);
        match self {
            Self::Anthropic => {
                let mut config = nanus_adapter_anthropic::AnthropicConfig::with_base_url(
                    self.model(),
                    "fictional-key",
                    base,
                );
                if let Some(limits) = limits {
                    config.set_response_limits(limits);
                }
                nanus_adapter_anthropic::AnthropicLlm::new(config)
                    .expect("adapter")
                    .stream_chat(request)
            }
            Self::DeepSeek => {
                let mut config = nanus_adapter_deepseek::DeepSeekConfig::with_base_url(
                    self.model(),
                    "fictional-key",
                    base,
                );
                if let Some(limits) = limits {
                    config.set_response_limits(limits);
                }
                nanus_adapter_deepseek::DeepSeekLlm::new(config)
                    .expect("adapter")
                    .stream_chat(request)
            }
            Self::Chat | Self::Responses => {
                let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
                    nanus_adapter_openai::Vendor::OpenAi,
                    self.model(),
                    "fictional-key",
                    base,
                );
                if let Some(limits) = limits {
                    config.set_response_limits(limits);
                }
                nanus_adapter_openai::OpenAiLlm::new(config)
                    .expect("adapter")
                    .stream_chat(request)
            }
        }
    }
    const fn model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-5-5",
            Self::DeepSeek => "deepseek-flash",
            Self::Chat => "gpt-5.3-codex",
            Self::Responses => "gpt-6.1-sol",
        }
    }
    const fn text(self) -> &'static str {
        match self {
            Self::Anthropic => {
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n"
            }
            Self::DeepSeek | Self::Chat => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n"
            }
            Self::Responses => {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n"
            }
        }
    }
    const fn end(self) -> &'static str {
        match self {
            Self::Anthropic => "data: {\"type\":\"message_stop\"}\n",
            Self::DeepSeek | Self::Chat => "data: [DONE]\n",
            Self::Responses => "data: {\"type\":\"response.completed\"}\n",
        }
    }
}
fn serve(status: u16, body: &[u8], keep_open: bool) -> (String, std::sync::mpsc::Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let body = body.to_vec();
    let (sent, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("connection");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        read_request(&mut socket);
        let extra = usize::from(keep_open);
        let length = body.len().saturating_add(extra);
        write!(socket,"HTTP/1.1 {status} OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").expect("head");
        let _ = socket.write_all(&body);
        let _ = socket.flush();
        if keep_open {
            let mut byte = [0];
            let dropped =
                matches!(socket.read(&mut byte), Ok(0)) || socket.peek(&mut byte).is_err();
            let _ = sent.send(dropped);
        }
    });
    (format!("http://{address}"), closed)
}
fn read_request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = socket.read(&mut buffer).expect("request");
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= end.saturating_add(4).saturating_add(length) {
                return;
            }
        }
        assert!(bytes.len() < 65536);
    }
}
async fn events(
    grammar: Grammar,
    status: u16,
    body: &[u8],
    policy: Option<ResponseLimits>,
) -> Vec<LlmEvent> {
    let (base, _) = serve(status, body, false);
    tokio::time::timeout(
        Duration::from_secs(3),
        grammar.stream(&base, policy).collect(),
    )
    .await
    .expect("stream ends")
}
fn only_error(events: &[LlmEvent], contains: &str) {
    assert!(
        matches!(events,[LlmEvent::ResponseHead,LlmEvent::Error(message)] if message.contains(contains)),
        "{events:?}"
    );
}
#[tokio::test]
async fn complete_bounded_streams_finish_on_every_grammar() {
    for grammar in GRAMMARS {
        let body = format!("{}{}", grammar.text(), grammar.end());
        let events = events(grammar, 200, body.as_bytes(), Some(limits())).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event,LlmEvent::TextDelta(text) if text=="hello")),
            "{grammar:?}: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, LlmEvent::Finished { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Error(_)))
        );
    }
}
#[tokio::test]
async fn bounded_eof_refuses_partial_content_while_stock_eof_still_finishes() {
    for grammar in GRAMMARS {
        let bounded = events(grammar, 200, grammar.text().as_bytes(), Some(limits())).await;
        assert!(bounded.iter().any(
            |event| matches!(event,LlmEvent::Error(message) if message.contains("termination"))
        ));
        assert!(!bounded.iter().any(|event| matches!(
            event,
            LlmEvent::Finished { .. } | LlmEvent::AssistantReplay(_)
        )));
        let stock = events(grammar, 200, grammar.text().as_bytes(), None).await;
        assert!(
            stock
                .iter()
                .any(|event| matches!(event, LlmEvent::Finished { .. })),
            "{grammar:?}: {stock:?}"
        );
    }
}
#[tokio::test]
async fn bounded_transport_refuses_unterminated_lines_before_json_decoding() {
    for grammar in GRAMMARS {
        only_error(
            &events(grammar, 200, &[b'x'; 1025], Some(limits())).await,
            "SSE line bytes",
        );
    }
}
#[tokio::test]
async fn bounded_transport_refuses_non_json_and_invalid_utf8() {
    for grammar in GRAMMARS {
        only_error(
            &events(grammar, 200, b"data: nope\n", Some(limits())).await,
            "not JSON",
        );
        only_error(
            &events(grammar, 200, b"data: \xff\n", Some(limits())).await,
            "UTF-8",
        );
    }
}
#[tokio::test]
async fn oversized_error_bodies_are_not_read_then_excerpted() {
    for grammar in GRAMMARS {
        only_error(
            &events(grammar, 429, &[b'x'; 257], Some(limits())).await,
            "HTTP error body bytes",
        );
        let exact = events(grammar, 429, &[b'x'; 256], Some(limits())).await;
        only_error(&exact, "429");
        let stock = events(grammar, 429, &[b'x'; 257], None).await;
        only_error(&stock, "429");
    }
}
#[tokio::test]
async fn bounded_streams_count_unknown_payloads_and_raw_comments() {
    for grammar in GRAMMARS {
        let policy = ResponseLimits::new(1024, 512, 8192, 2, 2, 256).expect("limits");
        only_error(
            &events(
                grammar,
                200,
                b"data: {}\ndata: {}\ndata: {}\n",
                Some(policy),
            )
            .await,
            "response events",
        );
        let policy = ResponseLimits::new(1024, 512, 1024, 32, 2, 256).expect("limits");
        only_error(
            &events(grammar, 200, &b": comment\n".repeat(103), Some(policy)).await,
            "response bytes",
        );
    }
}
#[tokio::test]
async fn terminal_events_and_limits_release_the_socket_before_outer_stream_drop() {
    for grammar in GRAMMARS {
        for (status, body) in [
            (
                200,
                format!("{}{}", grammar.text(), grammar.end()).into_bytes(),
            ),
            (200, vec![b'x'; 1025]),
            (429, vec![b'x'; 257]),
        ] {
            let (base, closed) = serve(status, &body, true);
            let mut stream = grammar.stream(&base, Some(limits()));
            loop {
                let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
                    .await
                    .expect("no wait for EOF")
                    .expect("terminal event");
                if matches!(event, LlmEvent::Finished { .. } | LlmEvent::Error(_)) {
                    break;
                }
            }
            assert!(
                peer_closed(closed).await,
                "{grammar:?}: socket remains open"
            );
            assert!(stream.next().await.is_none());
        }
    }
}
#[tokio::test]
async fn dropping_a_quiet_stream_releases_its_pending_response() {
    for grammar in GRAMMARS {
        let (base, closed) = serve(200, b"", true);
        let mut stream = grammar.stream(&base, Some(limits()));
        assert!(matches!(stream.next().await, Some(LlmEvent::ResponseHead)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.next())
                .await
                .is_err()
        );
        drop(stream);
        assert!(
            peer_closed(closed).await,
            "{grammar:?}: quiet response remains open"
        );
    }
}

async fn peer_closed(closed: std::sync::mpsc::Receiver<bool>) -> bool {
    // Hyper's connection task must keep being polled after its response is dropped.
    tokio::task::spawn_blocking(move || closed.recv_timeout(Duration::from_secs(2)))
        .await
        .expect("observer thread")
        .expect("peer closes before outer stream drop")
}

#[tokio::test]
async fn protocol_termination_does_not_require_a_trailing_newline() {
    for grammar in GRAMMARS {
        let body = format!("{}{}", grammar.text(), grammar.end().trim_end_matches('\n'));
        let events = events(grammar, 200, body.as_bytes(), Some(limits())).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Finished { .. })),
            "{grammar:?}: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Error(_)))
        );
    }
}

#[tokio::test]
async fn stock_deepseek_error_excerpts_keep_their_original_bytes_and_envelope() {
    let body = format!("{}x", "é".repeat(1000));
    let result = events(Grammar::DeepSeek, 429, body.as_bytes(), None).await;
    only_error(&result, "1 bytes omitted");
    assert!(matches!(&result[1],LlmEvent::Error(message) if message.contains(&"é".repeat(1000))));
    let body = r#"{"error":{"message":"original"}}"#;
    let result = events(Grammar::DeepSeek, 429, body.as_bytes(), None).await;
    only_error(&result, body);
    let result = events(Grammar::DeepSeek, 429, body.as_bytes(), Some(limits())).await;
    only_error(&result, body);
}
