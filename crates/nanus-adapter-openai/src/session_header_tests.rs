//! The subscription's `session-id` header, over a real socket: sent with the body's cache key on
//! the subscription's Responses path, by both entry points, and on no other path.
//!
//! The listener keeps each request's head, so the assertions are about the headers that reached
//! the server rather than what the adapter meant to send.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::time::Duration;

use futures::StreamExt as _;
use nanus_domain::Message;
use nanus_ports::{
    ChatRequest, LlmEvent, ManagedRequest, PreparedModelCall as _, PromptCacheRetention,
};
use serde_json::{Value, json};

use crate::{OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, Vendor};

const MODEL: &str = "gpt-5.6-sol";
/// Lowercase, because the listener lowercases the head it keeps.
const KEY: &str = "0190f2c4-session";

/// What one request carried: its head, lowercased, and its body.
struct Received {
    head: String,
    body: Value,
}

impl Received {
    /// The value of the `session-id` header, if the request carried one.
    fn session(&self) -> Option<&str> {
        self.head
            .lines()
            .find_map(|line| line.strip_prefix("session-id: "))
    }
}

/// Reads one request: its head up to the blank line, then `content-length` bytes of body.
fn read_request(stream: &mut std::net::TcpStream) -> Received {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 8192];
    let (start, length) = loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "the request ended before its head");
        received.extend_from_slice(&chunk[..count]);
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap();
            break (end.saturating_add(4), length);
        }
    };
    while received.len() < start.saturating_add(length) {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "the request ended before its body");
        received.extend_from_slice(&chunk[..count]);
    }
    let head = String::from_utf8_lossy(&received[..start]).to_ascii_lowercase();
    let body = serde_json::from_slice(&received[start..start.saturating_add(length)]).unwrap();
    Received { head, body }
}

/// Serves one request with a completed Responses stream and hands back what it received.
fn serve_one() -> (String, std::thread::JoinHandle<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let received = read_request(&mut stream);
        let sse = format!(
            "data: {}\n\n",
            json!({ "type": "response.completed", "response": {} })
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            sse.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(sse.as_bytes()).unwrap();
        received
    });
    (origin, worker)
}

fn subscription() -> OpenAiLlm {
    let mut config =
        OpenAiConfig::with_base_url(Vendor::OpenAi, MODEL, "k", OPENAI_SUBSCRIPTION_BASE_URL);
    config.set_protocol(Protocol::Responses);
    config.set_account_id("account");
    OpenAiLlm::new(config).unwrap()
}

/// A keyed request that asks for caching, for `model`.
fn keyed(model: &str) -> ChatRequest {
    ChatRequest::new(
        model,
        vec![Message::system("you are nanus"), Message::user("hello")],
    )
    .with_max_tokens(4_096)
    .with_prompt_cache(PromptCacheRetention::Short)
    .with_prompt_cache_key(KEY)
}

/// Sends `request` through the ordinary dispatch, to a local listener in place of the host.
async fn ordinary(llm: &OpenAiLlm, request: &ChatRequest) -> Received {
    let (protocol, payload, decoder) = llm.prepare_dispatch(request).unwrap();
    let (origin, worker) = serve_one();
    let endpoint = format!("{origin}{}", protocol.path());
    let _: Vec<LlmEvent> = llm
        .transmit(request, &payload, decoder, &endpoint)
        .collect()
        .await;
    worker.join().unwrap()
}

/// Sends `request` through the managed preparation, to a local listener in place of the host.
async fn managed(llm: &OpenAiLlm, request: ChatRequest) -> Received {
    let mut prepared = crate::managed::prepare(
        llm,
        ManagedRequest {
            request,
            selection_epoch: 0,
        },
    )
    .unwrap();
    let (origin, worker) = serve_one();
    prepared.retarget(format!("{origin}/responses"));
    let _: Vec<LlmEvent> = Box::new(prepared).stream().collect().await;
    worker.join().unwrap()
}

#[tokio::test]
async fn the_subscription_sends_its_cache_key_as_the_session_header_from_both_entry_points() {
    let llm = subscription();
    let sent = ordinary(&llm, &keyed(MODEL)).await;
    assert_eq!(sent.session(), Some(KEY));
    assert_eq!(sent.body["prompt_cache_key"], json!(KEY));
    assert!(sent.head.contains("chatgpt-account-id: account\r\n"));
    let sent = managed(&llm, keyed(MODEL)).await;
    assert_eq!(sent.session(), Some(KEY));
    assert_eq!(sent.body["prompt_cache_key"], json!(KEY));
}

/// The header is sent exactly when the body's key is: never unasked, never without a key, and
/// never a key that cannot be a header value, which still reaches the body.
#[tokio::test]
async fn the_subscription_sends_no_session_header_when_it_sends_no_cache_key() {
    let llm = subscription();
    let unasked = keyed(MODEL).with_prompt_cache(PromptCacheRetention::None);
    let sent = ordinary(&llm, &unasked).await;
    assert_eq!(sent.session(), None);
    assert!(sent.body.get("prompt_cache_key").is_none());
    let sent = managed(&llm, unasked).await;
    assert_eq!(sent.session(), None);

    let mut unkeyed = keyed(MODEL);
    unkeyed.prompt_cache_key = None;
    let sent = ordinary(&llm, &unkeyed).await;
    assert_eq!(sent.session(), None);

    let unsendable = keyed(MODEL).with_prompt_cache_key("line\nbreak");
    let sent = ordinary(&llm, &unsendable).await;
    assert_eq!(sent.session(), None);
    assert_eq!(sent.body["prompt_cache_key"], json!("line\nbreak"));
}

/// The API plan takes the key in the body and no header, on either wire; a gateway configured as
/// `OpenAI`, z.ai and Inception take neither.
#[tokio::test]
async fn no_other_path_is_sent_a_session_header() {
    let api = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, MODEL, "k")).unwrap();
    let sent = ordinary(&api, &keyed(MODEL)).await;
    assert_eq!(
        sent.body["prompt_cache_key"],
        json!(KEY),
        "the API's Responses key"
    );
    assert_eq!(sent.session(), None);
    assert_eq!(managed(&api, keyed(MODEL)).await.session(), None);
    let chat = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "k")).unwrap();
    let sent = ordinary(&chat, &keyed("gpt-5")).await;
    assert_eq!(
        sent.body["prompt_cache_key"],
        json!(KEY),
        "the API's chat key"
    );
    assert_eq!(sent.session(), None);

    let mut gateway =
        OpenAiConfig::with_base_url(Vendor::OpenAi, MODEL, "k", "http://127.0.0.1:9/v1");
    gateway.set_protocol(Protocol::Responses);
    let gateway = OpenAiLlm::new(gateway).unwrap();
    let sent = ordinary(&gateway, &keyed(MODEL)).await;
    assert!(sent.body.get("prompt_cache_key").is_none());
    assert_eq!(sent.session(), None);

    let zai = OpenAiLlm::new(OpenAiConfig::new(Vendor::Zai, "glm-5.2", "k")).unwrap();
    assert_eq!(ordinary(&zai, &keyed("glm-5.2")).await.session(), None);
    let model = crate::inception::MODELS[0];
    let inception = OpenAiLlm::new(OpenAiConfig::new(Vendor::Inception, model, "k")).unwrap();
    assert_eq!(ordinary(&inception, &keyed(model)).await.session(), None);
}
