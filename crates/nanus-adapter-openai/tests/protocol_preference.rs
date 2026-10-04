//! Exact wire selection, endpoint-scoped capabilities and refusal before HTTP.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use futures::StreamExt as _;
use nanus_adapter_openai::{
    OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol,
    ProtocolPreference, Vendor,
};
use nanus_domain::{ContentBlock, Message, ToolCallId};
use nanus_ports::{ChatRequest, ImageInputSupport, LlmError, LlmEvent, LlmPort as _};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn config() -> OpenAiConfig {
    OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional-key")
}
fn exact(config: &mut OpenAiConfig, protocol: Protocol) {
    config
        .set_protocol_preference(ProtocolPreference::Exact(protocol))
        .expect("supported wire");
}
fn image_request() -> ChatRequest {
    use base64::Engine as _;
    let pixels = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
    ChatRequest::new(
        "gpt-6-astra",
        vec![Message::Tool {
            call_id: ToolCallId::new("inspect"),
            content: "fictional image".into(),
            content_blocks: Some(vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(pixels),
            }]),
            is_error: false,
        }],
    )
}
#[test]
fn automatic_is_stock_routing_and_exact_wins_over_model_and_fallback() {
    let mut config = config();
    assert_eq!(config.protocol_preference(), ProtocolPreference::Automatic);
    assert_eq!(
        config.resolve_protocol("gpt-6-astra").unwrap(),
        Protocol::Responses
    );
    assert_eq!(
        config.resolve_protocol("gpt-5").unwrap(),
        Protocol::ChatCompletions
    );
    exact(&mut config, Protocol::ChatCompletions);
    config.set_protocol(Protocol::Responses);
    for model in ["gpt-6-astra", "gpt-6.9-future", "gpt-5"] {
        assert_eq!(
            config.resolve_protocol(model).unwrap(),
            Protocol::ChatCompletions
        );
    }
    exact(&mut config, Protocol::Responses);
    assert_eq!(
        config.resolve_protocol("gpt-5").unwrap(),
        Protocol::Responses
    );
    config
        .set_protocol_preference(ProtocolPreference::Automatic)
        .unwrap();
    assert_eq!(
        config.resolve_protocol("gpt-5").unwrap(),
        Protocol::Responses
    );
    config.set_protocol(Protocol::ChatCompletions);
    assert_eq!(
        config.resolve_protocol("gpt-5").unwrap(),
        Protocol::ChatCompletions
    );
}
#[test]
fn incompatible_preferences_refuse_without_changing_configuration() {
    let mut zai = OpenAiConfig::new(Vendor::Zai, "glm-5.3", "fictional-key");
    assert!(matches!(
        zai.set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses)),
        Err(LlmError::Unsupported { .. })
    ));
    assert_eq!(zai.protocol_preference(), ProtocolPreference::Automatic);
    exact(&mut zai, Protocol::ChatCompletions);
    let mut subscription = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-6-astra",
        "fictional-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    assert!(matches!(
        subscription.set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions)),
        Err(LlmError::Unsupported { .. })
    ));
    assert_eq!(
        subscription.protocol_preference(),
        ProtocolPreference::Automatic
    );
    exact(&mut subscription, Protocol::Responses);
}
#[test]
fn an_endpoint_edit_cannot_bypass_exact_protocol_validation() {
    let mut config = config();
    exact(&mut config, Protocol::ChatCompletions);
    config
        .set_base_url(OPENAI_SUBSCRIPTION_BASE_URL)
        .expect("endpoint edit");
    assert!(matches!(
        config.resolve_protocol("gpt-6-astra"),
        Err(LlmError::Unsupported { .. })
    ));
    assert!(matches!(
        OpenAiLlm::new(config),
        Err(nanus_adapter_openai::OpenAiError::UnsupportedProtocol)
    ));
    assert!(!nanus_adapter_openai::OpenAiError::UnsupportedProtocol.is_retryable());
}
#[test]
fn capability_evidence_belongs_to_the_selected_wire_and_endpoint() {
    for endpoint in [OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL] {
        let mut config =
            OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-6-astra", "fictional-key", endpoint);
        exact(&mut config, Protocol::Responses);
        let adapter = OpenAiLlm::new(config).expect("adapter");
        for model in Vendor::OpenAi.models() {
            let caps = adapter.capabilities(model);
            assert_eq!(
                caps.image_input,
                ImageInputSupport::Supported,
                "{endpoint} {model}"
            );
            assert_eq!(caps.require_image_profile(model).unwrap().model(), *model);
            assert!(caps.max_output_tokens.is_some());
        }
        assert_eq!(
            adapter.capabilities("gpt-6.2-sol"),
            nanus_ports::ModelCapabilities::default()
        );
    }
    let mut chat = config();
    exact(&mut chat, Protocol::ChatCompletions);
    let chat = OpenAiLlm::new(chat).expect("chat adapter");
    assert_eq!(
        chat.capabilities("gpt-6-astra"),
        nanus_ports::ModelCapabilities::default()
    );
    for endpoint in [
        "https://proxy.example.test/v1",
        "http://api.openai.com/v1",
        "https://api.openai.com/v1?x=1",
        "https://api.openai.com.evil.test/v1",
    ] {
        let adapter = OpenAiLlm::new(OpenAiConfig::with_base_url(
            Vendor::OpenAi,
            "gpt-6-astra",
            "fictional-key",
            endpoint,
        ))
        .expect("custom adapter");
        assert_eq!(
            adapter.capabilities("gpt-6-astra"),
            nanus_ports::ModelCapabilities::default()
        );
    }
}
#[tokio::test]
async fn exact_chat_image_requests_refuse_estimation_and_stream_dispatch() {
    let mut config = config();
    exact(&mut config, Protocol::ChatCompletions);
    let adapter = OpenAiLlm::new(config).expect("adapter");
    let request = image_request();
    assert_eq!(
        adapter.capabilities(&request.model).image_input,
        ImageInputSupport::Unknown
    );
    // Raw wire inspection remains available; it does not promote Chat capability.
    assert!(adapter.encode(&request).get("messages").is_some());
    assert!(matches!(
        adapter.estimate_request(&request),
        Err(LlmError::Unsupported { .. })
    ));
    let events: Vec<_> = adapter.stream_chat(request).collect().await;
    assert!(matches!(events.as_slice(),[LlmEvent::Error(message)] if message.contains("Unknown")));
}
#[tokio::test]
async fn a_custom_endpoint_cannot_inherit_a_promoted_image_profile_or_contact_http() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut config =
        OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-6-astra", "fictional-key", endpoint);
    exact(&mut config, Protocol::Responses);
    let adapter = OpenAiLlm::new(config).expect("adapter");
    let request = image_request();
    assert!(adapter.estimate_request(&request).is_err());
    let events: Vec<_> = adapter.stream_chat(request).collect().await;
    assert!(matches!(events.as_slice(),[LlmEvent::Error(message)] if message.contains("Unknown")));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
#[test]
fn exact_responses_output_controls_follow_the_endpoint_not_the_fallback() {
    let request =
        ChatRequest::new("gpt-6-astra", vec![Message::user("hello")]).with_max_tokens(8192);
    let mut public = config();
    public.set_protocol(Protocol::Responses);
    exact(&mut public, Protocol::Responses);
    let public = OpenAiLlm::new(public).expect("public adapter");
    assert_eq!(public.encode(&request)["max_output_tokens"], 8192);
    assert_eq!(
        public
            .estimate_request(&request)
            .expect("estimate")
            .reservation,
        8192
    );
    let mut subscription = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-6-astra",
        "fictional-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    exact(&mut subscription, Protocol::Responses);
    let subscription = OpenAiLlm::new(subscription).expect("subscription adapter");
    assert!(
        subscription
            .encode(&request)
            .get("max_output_tokens")
            .is_none()
    );
    assert!(matches!(
        subscription.estimate_request(&request),
        Err(LlmError::Unsupported { .. })
    ));
}
#[tokio::test]
async fn an_exact_endpoint_cannot_silently_discard_the_requested_output_ceiling() {
    let mut config = OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "gpt-6-astra",
        "fictional-key",
        OPENAI_SUBSCRIPTION_BASE_URL,
    );
    exact(&mut config, Protocol::Responses);
    let adapter = OpenAiLlm::new(config).unwrap();
    let request =
        ChatRequest::new("gpt-6-astra", vec![Message::user("hello")]).with_max_tokens(8192);
    assert!(adapter.estimate_request(&request).is_err());
    let events: Vec<_> = adapter.stream_chat(request).collect().await;
    assert!(
        matches!(events.as_slice(),[LlmEvent::Error(message)] if message.contains("output ceiling"))
    );
}
fn serve(
    protocol: Protocol,
) -> (
    String,
    std::sync::mpsc::Receiver<String>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, seen) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request = read_request(&mut socket);
        sent.send(request).unwrap();
        let body = match protocol {
            Protocol::ChatCompletions => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"exact chat\"}}]}\ndata: [DONE]\n"
            }
            Protocol::Responses => {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"exact responses\"}\ndata: {\"type\":\"response.completed\"}\n"
            }
        };
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    (endpoint, seen, task)
}
fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
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
                return String::from_utf8(bytes).unwrap();
            }
        }
        assert!(bytes.len() < 65536);
    }
}
#[tokio::test]
async fn exact_wire_paths_payloads_and_decoders_do_not_follow_model_switches() {
    for (protocol, model) in [
        (Protocol::ChatCompletions, "gpt-6-astra"),
        (Protocol::Responses, "gpt-5"),
    ] {
        let (endpoint, seen, task) = serve(protocol);
        let mut config = OpenAiConfig::with_base_url(
            Vendor::OpenAi,
            "configured-model",
            "fictional-key",
            endpoint,
        );
        exact(&mut config, protocol);
        config.set_response_limits(
            nanus_ports::ResponseLimits::new(1024, 1024, 8192, 16, 2, 256).unwrap(),
        );
        let adapter = OpenAiLlm::new(config).unwrap();
        assert!(adapter.endpoint_for(model).ends_with(protocol.path()));
        let request = ChatRequest::new(model, vec![Message::user("hello")]).with_max_tokens(8192);
        let events: Vec<_> = tokio::time::timeout(
            Duration::from_secs(3),
            adapter.stream_chat(request).collect(),
        )
        .await
        .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Finished { .. })),
            "{events:?}"
        );
        let expected = if protocol == Protocol::ChatCompletions {
            "exact chat"
        } else {
            "exact responses"
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event,LlmEvent::TextDelta(text) if text==expected))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Error(_)))
        );
        let request = seen.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            request.starts_with(&format!("POST {} ", protocol.path())),
            "{request}"
        );
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(body.get("input").is_some(), protocol == Protocol::Responses);
        assert_eq!(
            body.get("messages").is_some(),
            protocol == Protocol::ChatCompletions
        );
        if protocol == Protocol::Responses {
            assert_eq!(body["max_output_tokens"], 8192);
        }
        task.join().unwrap();
    }
}
