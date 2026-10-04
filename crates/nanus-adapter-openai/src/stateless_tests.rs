//! Actual admitted body/decoder/HTTP/runner/reload/fitting paths, with a fixture-only socket URL.
//! Public dispatch still derives its fixed endpoint from the checked configuration.
use super::*;
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, Message, Session, SessionId, ToolCall, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolRegistry, ToolResult, ToolSchema,
};
use nanus_ports::{ClockPort, ToolAdmission, ToolBatchProjection, ToolBatchReservation};
use serde_json::{Value, json};
use std::cell::Cell;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn adapter() -> OpenAiLlm {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    config.set_function_strictness(Some(false));
    config.set_stateless_responses(true);
    config.set_response_limits(
        nanus_ports::ResponseLimits::new(8192, 8192, 262_144, 100, 16, 1024).unwrap(),
    );
    OpenAiLlm::new(config).unwrap()
}

fn original_items() -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"rs1","summary":[],
        "encrypted_content":"opaque+fictional=","status":"completed"}),
        json!({"type":"function_call","id":"fc1","call_id":"c1","name":"read",
        "arguments":"{\"path\":\"a\"}","status":"completed"}),
        json!({"type":"function_call","id":"fc2","call_id":"c2","name":"read",
        "arguments":"{\"path\":\"b\"}","status":"completed"}),
    ]
}

fn frames(items: &[Value]) -> Vec<Value> {
    let mut frames = vec![json!({"type":"response.created","response":{
        "id":"response","status":"in_progress"}})];
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        match item["type"].as_str().unwrap() {
            "reasoning" => added["encrypted_content"] = Value::Null,
            "function_call" => added["arguments"] = json!(""),
            "message" => added["content"] = json!([]),
            _ => unreachable!(),
        }
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "function_call" {
            frames.push(json!({"type":"response.function_call_arguments.delta",
            "output_index":index,"item_id":item["id"],"delta":item["arguments"]}));
        }
        if item["type"] == "message" {
            frames.push(json!({"type":"response.output_text.delta",
            "output_index":index,"content_index":0,"item_id":item["id"],"delta":"Fictional answer."}));
        }
        frames.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    frames.push(json!({"type":"response.completed","response":{
        "id":"response","status":"completed","output":items}}));
    frames
}

fn answer() -> Vec<Value> {
    frames(&[json!({"type":"message","id":"answer","role":"assistant",
        "phase":"final_answer","status":"completed","content":[{"type":"output_text",
        "text":"Fictional answer.","annotations":[]}]})])
}

fn read_request(socket: &mut TcpStream) -> Value {
    // macOS inherits the listener's nonblocking mode; reads use the explicit timeout instead.
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while headers.len() < 8192 && !headers.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        headers.extend_from_slice(&byte);
    }
    assert!(headers.ends_with(b"\r\n\r\n"));
    let header = String::from_utf8(headers).unwrap().to_lowercase();
    assert!(header.starts_with("post /responses "));
    assert!(header.contains("authorization: bearer fictional-key\r\n"));
    assert!(!header.contains("chatgpt-account-id"));
    let length = header
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length <= 128 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn write_frames(socket: &mut TcpStream, frames: &[Value]) {
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
    let mut body = Vec::new();
    for frame in frames {
        body.extend_from_slice(format!("data: {frame}\n\n").as_bytes());
    }
    for chunk in body.chunks(17) {
        socket
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .unwrap();
        socket.write_all(chunk).unwrap();
        socket.write_all(b"\r\n").unwrap();
    }
    socket.write_all(b"0\r\n\r\n").unwrap();
}

fn accept_fixture(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
    loop {
        match listener.accept() {
            Ok((socket, _)) => return socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "fixture accept timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn server(scripts: Vec<Vec<Value>>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for frames in scripts {
            let mut socket = accept_fixture(&listener);
            requests.push(read_request(&mut socket));
            write_frames(&mut socket, &frames);
        }
        requests
    });
    (format!("http://{address}/responses"), worker)
}

struct FixtureModel {
    adapter: Rc<OpenAiLlm>,
    endpoint: String,
}
impl LlmPort for FixtureModel {
    fn model(&self) -> &str {
        self.adapter.model()
    }
    fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
        self.adapter.capabilities(model)
    }
    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        self.adapter.estimate_request(request)
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        match self.adapter.prepare_dispatch(&request) {
            Ok((protocol, body, decoder)) => {
                assert_eq!(protocol, Protocol::Responses);
                self.adapter
                    .transmit(&request, &body, decoder, &self.endpoint)
            }
            Err(error) => error_stream(&error.to_string()),
        }
    }
}

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        123
    }
}
struct Execute(Rc<Cell<usize>>);
impl ToolExecutor for Execute {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        self.0.set(self.0.get().checked_add(1).unwrap());
        Box::pin(async move { ToolResult::success(call.id, json!({"fictional":true})) })
    }
}

fn runner(model: FixtureModel, executed: Rc<Cell<usize>>, budget: u32) -> AgentRunner {
    let mut tools = ToolRegistry::new();
    tools
        .register(
            ToolDefinition::new(
                ToolSchema {
                    name: ToolName::new("read").unwrap(),
                    description: "Fictional read".into(),
                    parameters: json!({"type":"object",
        "properties":{"path":{"type":"string"},"offset":{"type":"integer"}},
        "required":["path"]}),
                },
                Execute(executed),
            )
            .with_access(nanus_domain::ToolAccess::Read),
        )
        .unwrap();
    AgentRunner::new(
        Rc::new(Box::new(model)),
        ToolRegistryHandle::new(tools),
        "Fictional prompt",
        AgentConfig::new(4, 2, "gpt-6-astra", 16_384)
            .unwrap()
            .with_context_budget(budget)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(8192, 0)
}

struct Admit {
    adapter: Rc<OpenAiLlm>,
    count: Rc<Cell<usize>>,
}
struct Lease {
    adapter: Rc<OpenAiLlm>,
}
impl ToolAdmission for Admit {
    fn reserve(
        &self,
        p: &ToolBatchProjection<'_>,
    ) -> Result<Box<dyn ToolBatchReservation>, nanus_ports::AdmissionError> {
        let mut candidate = p.request.clone();
        for message in candidate.messages.iter_mut().rev().take(p.calls.len()) {
            if let Message::Tool {
                content,
                content_blocks,
                ..
            } = message
            {
                *content = "Reserved fictional success".repeat(20);
                *content_blocks = None;
            } else {
                panic!("final batch slot");
            }
        }
        assert!(
            (p.estimate)(&candidate)
                .unwrap()
                .fits(p.capabilities, &candidate)
        );
        assert!(self.adapter.prepare_responses(&candidate).is_err());
        self.count.set(self.count.get().checked_add(1).unwrap());
        Ok(Box::new(Lease {
            adapter: Rc::clone(&self.adapter),
        }))
    }
}
impl ToolBatchReservation for Lease {
    fn admit(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn before_dispatch(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn validate_result(
        &self,
        _: &ToolCall,
        _: &ToolResult,
        _: &ToolResult,
    ) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn commit(&self, request: &ChatRequest) -> Result<(), nanus_ports::AdmissionError> {
        assert!(self.adapter.prepare_responses(request).is_ok());
        Ok(())
    }
}

fn assert_wire_requests(requests: &[Value]) {
    assert_eq!(
        &requests[1]["input"].as_array().unwrap()[1..4],
        original_items()
    );
    assert_eq!(requests[1]["input"][4]["call_id"], "c1");
    assert_eq!(requests[1]["input"][5]["call_id"], "c2");
    assert_eq!(requests[2]["input"].as_array().unwrap().len(), 1);
    assert!(
        requests[2]["instructions"]
            .as_str()
            .unwrap()
            .contains("turn")
    );
    for request in requests {
        assert_eq!(request["store"], false);
        assert_eq!(request["max_output_tokens"], 8192);
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["tools"][0]["strict"], false);
        assert!(request.get("previous_response_id").is_none());
    }
}

#[tokio::test]
async fn actual_admission_transport_runner_and_reload_keep_original_items_and_fit_complete_turns() {
    let (endpoint, worker) = server(vec![frames(&original_items()), answer(), answer()]);
    let adapter = Rc::new(adapter());
    let executed = Rc::new(Cell::new(0));
    let admitted = Rc::new(Cell::new(0));
    let model = FixtureModel {
        adapter: Rc::clone(&adapter),
        endpoint: endpoint.clone(),
    };
    let first = runner(model, Rc::clone(&executed), 64_000).with_tool_admission(Rc::new(Admit {
        adapter: Rc::clone(&adapter),
        count: Rc::clone(&admitted),
    }));
    let mut session = Session::new(SessionId::new("fictional-http"), 123, "/fictional");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        first.run_turn(&mut session, &"x".repeat(40_000), &mut Silent, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.is_success());
    assert_eq!(executed.get(), 2);
    assert_eq!(admitted.get(), 1);
    let original = session.clone();
    let raw = session.try_to_jsonl().unwrap();
    let mut loaded = Session::from_jsonl(&raw).unwrap();
    assert_eq!(loaded, original);
    let second = runner(
        FixtureModel { adapter, endpoint },
        Rc::clone(&executed),
        16_384,
    );
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            second.run_turn(&mut loaded, "New fictional question", &mut Silent, None)
        )
        .await
        .unwrap()
        .unwrap()
        .is_success()
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert_wire_requests(&requests);
    assert!(loaded.try_to_jsonl().unwrap().contains(&"x".repeat(40_000)));
    assert_eq!(executed.get(), 2);
}

#[tokio::test]
async fn truncated_actual_transport_never_executes_pending_calls_or_retains_success_replay() {
    let mut incomplete = frames(&original_items());
    incomplete.pop();
    let (endpoint, worker) = server(vec![incomplete]);
    let executed = Rc::new(Cell::new(0));
    let runner = runner(
        FixtureModel {
            adapter: Rc::new(adapter()),
            endpoint,
        },
        Rc::clone(&executed),
        64_000,
    );
    let mut session = Session::new(SessionId::new("fictional-truncated"), 123, "/fictional");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            runner.run_turn(&mut session, "Fictional question", &mut Silent, None)
        )
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(executed.get(), 0);
    assert_eq!(worker.join().unwrap().len(), 1);
    assert!(!session.derive_messages().iter().any(|message| matches!(
        message,
        Message::Assistant {
            replay: Some(_),
            ..
        }
    )));
    assert_eq!(
        Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap(),
        session
    );
}

#[tokio::test]
async fn opaque_only_actual_completion_is_replayed_after_runner_reload_and_next_user() {
    let original = original_items()[..1].to_vec();
    let (endpoint, worker) = server(vec![frames(&original), answer()]);
    let executed = Rc::new(Cell::new(0));
    let runner = runner(
        FixtureModel {
            adapter: Rc::new(adapter()),
            endpoint,
        },
        Rc::clone(&executed),
        64_000,
    );
    let mut session = Session::new(SessionId::new("fictional-opaque"), 123, "/fictional");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            runner.run_turn(&mut session, "First fictional question", &mut Silent, None)
        )
        .await
        .unwrap()
        .unwrap()
        .is_success()
    );
    let mut loaded = Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap();
    assert!(loaded.derive_messages().iter().any(|message|
        matches!(message,Message::Assistant {replay:Some(replay),..} if replay.blocks==original)));
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            runner.run_turn(&mut loaded, "Next fictional question", &mut Silent, None)
        )
        .await
        .unwrap()
        .unwrap()
        .is_success()
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["input"][1], original[0]);
    assert_eq!(
        requests[1]["input"][2]["content"][0]["text"],
        "Next fictional question"
    );
    assert_eq!(executed.get(), 0);
}

#[tokio::test]
async fn stateless_transport_refuses_redirect_without_contacting_a_second_origin() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let location = format!("http://{}/fictional-target", target.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut socket = accept_fixture(&listener);
        let request = read_request(&mut socket);
        socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        request
    });
    let adapter = adapter();
    let mut request = ChatRequest::new("gpt-6-astra", vec![Message::user("fictional")]);
    request.max_tokens = Some(8192);
    request.context_budget = Some(64_000);
    let (_, body, decoder) = adapter.prepare_dispatch(&request).unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(3),
        adapter
            .transmit(&request, &body, decoder, &endpoint)
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert!(
        matches!(&events[..],[LlmEvent::ResponseHead,LlmEvent::Error(message)]
        if message.contains("HTTP 302")),
        "{events:?}"
    );
    assert_eq!(worker.join().unwrap(), body);
    assert!(matches!(target.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}
