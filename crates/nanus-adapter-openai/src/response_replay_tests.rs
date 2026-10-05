//! The real HTTP/SSE decoder consumes completed replay and rejects truncated bodies.
use super::*;
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::time::Duration;

fn frames() -> Vec<Value> {
    let reasoning = json!({"id":"r","type":"reasoning","summary":[],
        "status":"completed","encrypted_content":"opaque-fixture"});
    let call = json!({"id":"i","type":"function_call","call_id":"c","name":"read",
        "arguments":"{}","status":"completed"});
    vec![
        json!({"type":"response.created","response":{"id":"response-1","status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"r","type":"reasoning","summary":[],"encrypted_content":null,"status":"in_progress"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        json!({"type":"response.output_item.added","output_index":1,"item":{
            "id":"i","type":"function_call","call_id":"c","name":"read",
            "arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"i","delta":"{}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":call}),
        json!({"type":"response.completed","response":{"id":"response-1",
            "status":"completed","output":[reasoning,call]}}),
    ]
}

fn server(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fictional local server");
    let address = listener.local_addr().expect("address");
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("client");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("write timeout");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while request.len() < 16_384 && !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).expect("request headers");
            request.extend_from_slice(&byte);
        }
        assert!(request.ends_with(b"\r\n\r\n"));
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").expect("head");
        for fragment in body.chunks(17) {
            let header = format!("{:x}\r\n", fragment.len());
            if socket.write_all(header.as_bytes()).is_err()
                || socket.write_all(fragment).is_err()
                || socket.write_all(b"\r\n").is_err()
            {
                return;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n");
    });
    (format!("http://{address}"), worker)
}

async fn observe(frames: &[Value], limits: ResponseLimits) -> Vec<LlmEvent> {
    let mut body = Vec::new();
    for frame in frames {
        body.extend_from_slice(b"data: ");
        body.extend_from_slice(
            serde_json::to_string(frame)
                .expect("fixture JSON")
                .as_bytes(),
        );
        body.extend_from_slice(b"\n\n");
    }
    let (endpoint, worker) = server(body);
    let response = reqwest::Client::new()
        .get(&endpoint)
        .send()
        .await
        .expect("fixture response");
    let accumulator = crate::responses::StreamAccumulator::with_prefix("a".repeat(64), limits)
        .expect("admitted digest");
    let stream = decode(
        response,
        endpoint,
        Vendor::OpenAi,
        Decoder::Responses(Box::new(accumulator)),
        Some(limits),
    );
    let events = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("bounded fixture stream");
    worker.join().expect("server completed");
    events
}

#[tokio::test]
async fn fragmented_http_preserves_empty_encrypted_reasoning_and_releases_calls_only_after_replay()
{
    let limits = ResponseLimits::new(8192, 8192, 32768, 100, 8, 1024).expect("limits");
    let events = observe(&frames(), limits).await;
    assert!(matches!(&events[0],LlmEvent::AssistantReplay(replay)
        if replay.blocks[0]["encrypted_content"]=="opaque-fixture"));
    assert!(matches!(&events[1],LlmEvent::ToolCallDelta { id:Some(id),.. } if id.as_str()=="c"));
    assert!(matches!(events[2], LlmEvent::Finished { .. }));
}

#[tokio::test]
async fn truncated_failed_and_over_budget_http_never_release_pending_calls_or_completion() {
    let limits = ResponseLimits::new(8192, 8192, 32768, 100, 8, 1024).expect("limits");
    let valid = frames();
    let mut failed = valid.clone();
    failed.pop();
    failed.push(json!({"type":"response.failed"}));
    for (frames, limits) in [
        (&valid[..6], limits),
        (&failed[..], limits),
        (
            &valid[..],
            ResponseLimits::new(64, 64, 64, 100, 8, 64).expect("small limits"),
        ),
    ] {
        let events = observe(frames, limits).await;
        assert!(matches!(&events[..], [LlmEvent::Error(_)]), "{events:?}");
    }
}
