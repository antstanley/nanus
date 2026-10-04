//! The provider wire: framing a streamed response, encoding a request, and the whole adapter
//! path from socket to events.
//!
//! The framing benchmarks are the per-token cost: every delta a model streams passes through
//! [`SseFrames`] or [`ResponseFrames`] before an adapter sees it. The encoding benchmarks are
//! the per-step cost, and the one that grows with the conversation: every model step encodes
//! the *whole* history, so a request body that is slow at a hundred turns is slow on every
//! step of a long session. The stream benchmarks run a real adapter against a loopback
//! server, so they price everything between the socket and the agent loop — HTTP framing,
//! SSE framing, JSON decoding, and tool-call reassembly — in one figure.

use std::rc::Rc;
use std::sync::Arc;

use core::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput};
use futures::StreamExt as _;
use nanus_adapter_anthropic::{AnthropicConfig, AnthropicLlm};
use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm};
use nanus_adapter_local::{LocalFs, LocalShell};
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Vendor};
use nanus_bench::{Metric, fixtures};
use nanus_domain::{Message, ToolSchema};
use nanus_ports::{
    ChatRequest, FsHandle, LlmEvent, LlmPort, ResponseFrames, ResponseLimits, ShellHandle,
    SseFrames,
};
use serde_json::{Value, json};
use std::hint::black_box;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;

/// Content deltas in a fixture response.
const DELTAS: usize = 2_000;

/// The reasoning deltas that precede them in a chat-completions response.
const REASONING_DELTAS: usize = 500;

/// A typical TCP segment payload: how a response body actually arrives off the network.
const SEGMENT: usize = 1_460;

/// The fragments a streamed tool call's arguments arrive in, splitting the JSON mid-value.
const ARGUMENTS: [&str; 4] = [
    r#"{"file_path":"#,
    r#""crates/nanus-domain/src/session.rs""#,
    r#","offset":1"#,
    r#","limit":80}"#,
];

/// One chat-completions chunk carrying `delta`, framed as the API sends it.
fn chunk(delta: &Value, finish: Option<&str>) -> String {
    let payload = json!({
        "id": "chatcmpl-bench",
        "object": "chat.completion.chunk",
        "created": 1_767_225_600,
        "model": "deepseek-flash",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    format!("data: {payload}\n\n")
}

/// A chat-completions body (`DeepSeek`, `OpenAI`): reasoning, an answer, a fragmented tool call,
/// the usage chunk, and the sentinel.
fn chat_body() -> String {
    let mut body = String::new();
    for _ in 0..REASONING_DELTAS {
        body.push_str(&chunk(&json!({ "reasoning_content": "so a hole " }), None));
    }
    for _ in 0..DELTAS {
        body.push_str(&chunk(&json!({ "content": "the log " }), None));
    }
    for (index, fragment) in ARGUMENTS.iter().enumerate() {
        let call = if index == 0 {
            json!({ "index": 0, "id": "call-1", "type": "function",
                    "function": { "name": "read", "arguments": fragment } })
        } else {
            json!({ "index": 0, "function": { "arguments": fragment } })
        };
        body.push_str(&chunk(&json!({ "tool_calls": [call] }), None));
    }
    let mut last = chunk(&json!({}), Some("tool_calls"));
    // The usage rides on the final chunk, beside the finish reason.
    last = last.replacen(
        "\"id\"",
        "\"usage\":{\"prompt_tokens\":14500,\"completion_tokens\":420,\
         \"prompt_tokens_details\":{\"cached_tokens\":13800},\
         \"completion_tokens_details\":{\"reasoning_tokens\":90}},\"id\"",
        1,
    );
    body.push_str(&last);
    body.push_str("data: [DONE]\n\n");
    body
}

/// One event-typed Messages API frame.
fn event(payload: &Value) -> String {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("ping");
    format!("event: {kind}\ndata: {payload}\n\n")
}

/// A Messages API body (Anthropic): an answer and a fragmented tool call, ending at
/// `message_stop` rather than at a sentinel.
fn messages_body() -> String {
    let mut body = event(&json!({ "type": "message_start", "message": {
        "id": "msg_bench", "usage": { "input_tokens": 14_500, "output_tokens": 1 } } }));
    body.push_str(&event(&json!({ "type": "content_block_start", "index": 0,
        "content_block": { "type": "text", "text": "" } })));
    for _ in 0..DELTAS {
        body.push_str(&event(&json!({ "type": "content_block_delta", "index": 0,
            "delta": { "type": "text_delta", "text": "the log " } })));
    }
    body.push_str(&event(&json!({ "type": "content_block_stop", "index": 0 })));
    body.push_str(&event(&json!({ "type": "content_block_start", "index": 1,
        "content_block": { "type": "tool_use", "id": "toolu_1", "name": "read", "input": {} } })));
    for fragment in ARGUMENTS {
        body.push_str(&event(&json!({ "type": "content_block_delta", "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": fragment } })));
    }
    body.push_str(&event(&json!({ "type": "content_block_stop", "index": 1 })));
    body.push_str(&event(&json!({ "type": "message_delta",
        "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 420 } })));
    body.push_str(&event(&json!({ "type": "message_stop" })));
    body
}

/// Budgets generous enough that the fixture never trips one, so the limited reader is
/// measured doing its accounting rather than failing.
fn limits() -> ResponseLimits {
    ResponseLimits::new(1 << 20, 1 << 20, 16 << 20, 100_000, 256, 64 << 10)
        .unwrap_or_else(|error| unreachable!("fixture limits: {error}"))
}

/// Feeds every chunk to a fresh framer, returning how many payloads came out.
fn frame_sse(chunks: &[&[u8]]) -> usize {
    let mut frames = SseFrames::new();
    chunks.iter().fold(0_usize, |count, chunk| {
        count.saturating_add(frames.push(chunk).len())
    })
}

fn frame_response(chunks: &[&[u8]], limits: Option<ResponseLimits>) -> usize {
    let mut frames = ResponseFrames::new(limits);
    chunks.iter().fold(0_usize, |count, chunk| {
        let payloads = frames
            .push(chunk)
            .unwrap_or_else(|error| unreachable!("fixture framing: {error}"));
        count.saturating_add(payloads.len())
    })
}

/// Shortens a timing group's sampling so the file's full run stays near two minutes.
///
/// Only the timing run is changed: the counting runs already use the short windows
/// `nanus_bench::counting` sets, and a longer one would only repeat an exact count. A group
/// setting outranks the command line, so `--measurement-time` does not shorten these.
fn windows<M: Metric>(group: &mut BenchmarkGroup<'_, M>, samples: usize, measurement: Duration) {
    if M::NAME == WallTime::NAME {
        group
            .sample_size(samples)
            .warm_up_time(Duration::from_secs(1))
            .measurement_time(measurement);
    }
}

/// SSE framing, fed in network segments and one frame per read: the two ends of how a body
/// can arrive, since the framer's cost depends on how often a line straddles a chunk.
fn sse<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("wire/sse"));
    windows(&mut group, 50, Duration::from_secs(2));
    let body = chat_body();
    group.throughput(Throughput::Bytes(body.len() as u64));
    let segments: Vec<&[u8]> = body.as_bytes().chunks(SEGMENT).collect();
    let frames: Vec<&[u8]> = body.split_inclusive("\n\n").map(str::as_bytes).collect();
    let limits = limits();
    for (feed, chunks) in [("segments", &segments), ("frames", &frames)] {
        group.bench_with_input(BenchmarkId::new("sse_frames", feed), chunks, |b, chunks| {
            b.iter(|| frame_sse(black_box(chunks)));
        });
        group.bench_with_input(BenchmarkId::new("legacy", feed), chunks, |b, chunks| {
            b.iter(|| frame_response(black_box(chunks), None));
        });
        group.bench_with_input(BenchmarkId::new("limited", feed), chunks, |b, chunks| {
            b.iter(|| frame_response(black_box(chunks), Some(limits)));
        });
    }
    group.finish();
}

/// The seven stock tools and the five goal tools: the schema block every request carries.
fn tool_schemas() -> Vec<ToolSchema> {
    let root = std::env::temp_dir();
    let fs = LocalFs::new(&root).unwrap_or_else(|error| unreachable!("temp dir: {error}"));
    let fs: FsHandle = Rc::new(Box::new(fs));
    let shell: ShellHandle = Rc::new(Box::new(LocalShell::unconfined(root)));
    let registry = nanus_bundle::build_toolset(&fs, &shell)
        .unwrap_or_else(|error| unreachable!("stock toolset: {error}"));
    let mut schemas: Vec<ToolSchema> = registry.schemas().into_iter().cloned().collect();
    schemas.extend(nanus_bundle::goal_tools::schemas());
    schemas
}

/// A request for a session of `turns` turns, as a model step would build it.
fn request(model: &str, turns: u32, tools: &[ToolSchema]) -> ChatRequest {
    let mut messages = vec![Message::system(fixtures::prose(4_000))];
    messages.extend(fixtures::session(turns).derive_messages());
    ChatRequest::new(model, messages).with_tools(tools.to_vec())
}

/// An adapter's request encoder, with the adapter it encodes for captured.
type Encoder = Box<dyn Fn(&ChatRequest) -> Value>;

/// The adapters whose encoders are measured, with the model that selects each wire.
///
/// `OpenAI` appears twice because the model picks the protocol: `gpt-5` is sent to chat
/// completions and `gpt-5.6` to the Responses API, and the two encoders share nothing.
fn encoders() -> Vec<(&'static str, &'static str, Encoder)> {
    let deepseek = DeepSeekLlm::new(DeepSeekConfig::new("deepseek-flash", "bench-key"))
        .unwrap_or_else(|error| unreachable!("deepseek adapter: {error}"));
    let anthropic = AnthropicLlm::new(AnthropicConfig::new("claude-sonnet-5-5", "bench-key"))
        .unwrap_or_else(|error| unreachable!("anthropic adapter: {error}"));
    let chat = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "bench-key"))
        .unwrap_or_else(|error| unreachable!("openai adapter: {error}"));
    let responses = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5.6", "bench-key"))
        .unwrap_or_else(|error| unreachable!("openai adapter: {error}"));
    vec![
        (
            "deepseek",
            "deepseek-flash",
            Box::new(move |r| deepseek.encode(r)),
        ),
        (
            "anthropic",
            "claude-sonnet-5-5",
            Box::new(move |r| anthropic.encode(r)),
        ),
        ("openai_chat", "gpt-5", Box::new(move |r| chat.encode(r))),
        (
            "openai_responses",
            "gpt-5.6",
            Box::new(move |r| responses.encode(r)),
        ),
    ]
}

/// Request encoding: the adapter's JSON body, then that body as the string sent on the wire.
fn encode<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("wire/encode"));
    windows(&mut group, 50, Duration::from_secs(2));
    let tools = tool_schemas();
    for (name, model, encoder) in encoders() {
        for turns in [10, 100] {
            let request = request(model, turns, &tools);
            let body = serde_json::to_string(&encoder(&request))
                .unwrap_or_else(|error| unreachable!("fixture body: {error}"));
            group.throughput(Throughput::Bytes(body.len() as u64));
            let id = format!("{name}/{turns}");
            group.bench_with_input(BenchmarkId::new("value", &id), &request, |b, request| {
                b.iter(|| encoder(black_box(request)));
            });
            group.bench_with_input(BenchmarkId::new("body", &id), &request, |b, request| {
                b.iter(|| serde_json::to_string(&encoder(black_box(request))));
            });
        }
    }
    group.finish();
}

/// The length of the request at the head of `buffer`, once all of it has arrived.
fn complete(buffer: &[u8]) -> Option<usize> {
    let head_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(buffer.get(..head_end)?).ok()?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let total = head_end.checked_add(4)?.checked_add(length)?;
    (buffer.len() >= total).then_some(total)
}

/// Answers every request on one kept-alive connection with the same scripted response.
async fn serve(mut stream: TcpStream, response: Arc<[u8]>) {
    let mut buffer = Vec::with_capacity(64 << 10);
    let mut read = vec![0_u8; 64 << 10];
    loop {
        let length = loop {
            if let Some(length) = complete(&buffer) {
                break length;
            }
            match stream.read(&mut read).await {
                Ok(0) | Err(_) => return,
                Ok(count) => buffer.extend_from_slice(read.get(..count).unwrap_or_default()),
            }
        };
        buffer.drain(..length);
        if stream.write_all(&response).await.is_err() {
            return;
        }
    }
}

/// Binds a loopback server on `runtime` that answers every request with `body` as SSE.
///
/// The server is a task on the benchmark's own current-thread runtime rather than a thread,
/// so nothing allocates concurrently with a measurement. Its share of each iteration is
/// reading one request into a reused buffer and writing a prebuilt response, so the counts
/// are the adapter's to within a constant.
fn loopback(runtime: &Runtime, body: &str) -> String {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    let response: Arc<[u8]> = [head.as_bytes(), body.as_bytes()].concat().into();
    let listener = runtime
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .unwrap_or_else(|error| unreachable!("loopback bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| unreachable!("loopback address: {error}"));
    runtime.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Without this, the response's last partial segment waits on the client's
            // delayed acknowledgement, and every iteration measures a timer rather than
            // the adapter.
            let _ = stream.set_nodelay(true);
            tokio::spawn(serve(stream, Arc::clone(&response)));
        }
    });
    format!("http://{address}")
}

/// Drives one request to the end of its stream, returning the events and any error.
async fn drain(llm: &dyn LlmPort, request: ChatRequest) -> (usize, Option<String>) {
    let mut stream = llm.stream_chat(request);
    let mut count = 0_usize;
    let mut failure = None;
    while let Some(event) = stream.next().await {
        if let LlmEvent::Error(message) = event {
            failure = Some(message);
        }
        count = count.saturating_add(1);
    }
    (count, failure)
}

type Build = fn(&str) -> Box<dyn LlmPort>;

fn deepseek_at(base: &str) -> Box<dyn LlmPort> {
    let config = DeepSeekConfig::with_base_url("deepseek-flash", "bench-key", base);
    Box::new(DeepSeekLlm::new(config).unwrap_or_else(|error| unreachable!("adapter: {error}")))
}

fn openai_at(base: &str) -> Box<dyn LlmPort> {
    let config = OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-5", "bench-key", base);
    Box::new(OpenAiLlm::new(config).unwrap_or_else(|error| unreachable!("adapter: {error}")))
}

fn anthropic_at(base: &str) -> Box<dyn LlmPort> {
    let config = AnthropicConfig::with_base_url("claude-sonnet-5-5", "bench-key", base);
    Box::new(AnthropicLlm::new(config).unwrap_or_else(|error| unreachable!("adapter: {error}")))
}

/// A whole response through a real adapter over loopback TCP, connection kept alive.
///
/// The request is a single user message so that the figure is the decode path, which is
/// what scales with the response; the encode path has its own group above.
fn stream<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("wire/stream"));
    windows(&mut group, 10, Duration::from_secs(3));
    let chat = chat_body();
    let messages = messages_body();
    let cases: [(&str, &str, &str, Build); 3] = [
        ("deepseek", "deepseek-flash", &chat, deepseek_at),
        ("openai_chat", "gpt-5", &chat, openai_at),
        ("anthropic", "claude-sonnet-5-5", &messages, anthropic_at),
    ];
    for (name, model, body, build) in cases {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| unreachable!("runtime: {error}"));
        let llm = build(&loopback(&runtime, body));
        let request = ChatRequest::new(model, vec![Message::user("Explain the session log.")]);
        // A stream that fails would be measured failing fast; check the fixture first.
        let (events, failure) = runtime.block_on(drain(llm.as_ref(), request.clone()));
        assert!(failure.is_none(), "{name} decodes the fixture: {failure:?}");
        assert!(
            events > DELTAS,
            "{name} streams every delta: {events} events"
        );
        group.throughput(Throughput::Bytes(body.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| runtime.block_on(drain(llm.as_ref(), black_box(request.clone()))));
        });
    }
    group.finish();
}

nanus_bench::benches!(sse, encode, stream);
