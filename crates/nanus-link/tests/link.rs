//! The link, end to end: a real socket, a real turn, a real session on disk.
//!
//! Everything here runs over `bind`ed sockets in a temporary directory rather than a
//! connected pair, because the parts most likely to be wrong are the ones a pair would
//! skip: the handshake, the framing across process boundaries, the permissions on the
//! socket file, and the recording that has to have happened by the time a client is told
//! the turn is over.
//!
//! The model is scripted, so the suite needs no credential and no network. The point is
//! the transport, not the provider.

#![cfg(feature = "server")]
// A panic in a test *is* the assertion, and a fixture with no sane default has nowhere
// else to put the failure. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::rc::Rc;

use nanus_adapter_local::SystemClock;
use nanus_adapter_store::JsonlStore;
use nanus_bundle::AgentRunner;
use nanus_domain::{AgentConfig, ToolRegistry};
use nanus_link::protocol::{AgentInfo, Frame, Request};
use nanus_link::server::{Agent, Parts};
use nanus_link::{Client, LinkError};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, LlmPort, LlmStream, StoreHandle};

/// A model that answers every request the same way, without a network.
struct ScriptedLlm;

impl LlmPort for ScriptedLlm {
    fn model(&self) -> &'static str {
        "scripted"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::iter(vec![
            LlmEvent::TextDelta("hello back".to_owned()),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]))
    }
}

/// Builds an agent over a store in `dir`, and returns the store alongside it.
fn scripted_agent(dir: &Path) -> (Agent, StoreHandle) {
    let store = nanus_kernel::runtime::block_on(async {
        JsonlStore::new(dir.to_path_buf())
            .await
            .expect("the store opens")
            .handle()
    });
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(ScriptedLlm));
    let config = AgentConfig::new(4, 1, "scripted", 4096).expect("a valid agent config");
    let runner = AgentRunner::new(llm, Rc::new(ToolRegistry::new()), "you are a test", config)
        .expect("a valid runner");
    let agent = Agent::from_parts(Parts {
        runner: Rc::new(runner),
        store: store.clone(),
        clock: SystemClock::new().handle(),
        workspace: dir.to_path_buf(),
        model: "scripted".to_owned(),
        tools: 0,
    });
    (agent, store)
}

/// Sends one prompt and returns every frame up to and including the ending.
async fn prompt(socket: &Path, text: &str) -> (AgentInfo, Vec<Frame>) {
    let mut client = Client::connect(socket).await.expect("the agent answers");
    let info = client.info().clone();
    client
        .send(&Request::Prompt {
            text: text.to_owned(),
        })
        .await
        .expect("the prompt is sent");
    let mut frames = Vec::new();
    while let Some(frame) = client.next().await.expect("frames are readable") {
        let last = frame.is_end_of_turn();
        frames.push(frame);
        if last {
            break;
        }
    }
    (info, frames)
}

/// The text a stream carried, concatenated in arrival order.
fn text_of(frames: &[Frame]) -> String {
    let mut text = String::new();
    for frame in frames {
        if let Frame::Text { delta } = frame {
            text.push_str(delta);
        }
    }
    text
}

#[test]
fn a_prompt_streams_an_answer_and_records_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (info, frames) = nanus_kernel::runtime::block_on_local(async move {
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, Rc::new(agent), async move {
                let _ = stop_rx.await;
            }));
        let exchanged = prompt(&socket_for_client, "say hello").await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        exchanged
    });

    assert_eq!(info.model, "scripted");
    assert_eq!(info.tools, 0, "the scripted agent offers no tools");
    assert_eq!(text_of(&frames), "hello back");
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::Done { answer } if answer == "hello back")),
        "the turn ends with the answer: {frames:?}"
    );

    // The contract the server keeps: by the time a client has seen the ending, the
    // session is already written down.
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1, "one connection is one session");
    assert!(
        listed.first().is_some_and(|row| row.event_count > 0),
        "the turn left events behind: {listed:?}"
    );
}

#[test]
fn two_connections_are_two_conversations() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    nanus_kernel::runtime::block_on_local(async move {
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, Rc::new(agent), async move {
                let _ = stop_rx.await;
            }));
        let (first, _) = prompt(&socket_for_client, "one").await;
        let (second, _) = prompt(&socket_for_client, "two").await;
        assert_ne!(
            first.session, second.session,
            "each connection starts its own session"
        );
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("clean");
    });

    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 2);
}

#[test]
fn a_status_request_describes_the_agent_without_running_a_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let described = nanus_kernel::runtime::block_on_local(async move {
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, Rc::new(agent), async move {
                let _ = stop_rx.await;
            }));
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let answer = client.ask_status().await;
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        answer
    });

    let described = described.expect("a status reply");
    assert_eq!(described.model, "scripted");
    assert!(!described.session.is_empty(), "the session is named");
}

#[test]
fn a_shutdown_request_stops_the_agent_by_itself() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    // No external stop signal at all: the only thing that can end this server is the
    // protocol. If the request were ignored, this test would hang rather than fail, so
    // it is also the test that proves `serve` is not waiting on something else.
    let served = nanus_kernel::runtime::block_on_local(async move {
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = tokio::task::spawn_local(nanus_link::serve(
            listener,
            Rc::new(agent),
            std::future::pending(),
        ));
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client
            .request_shutdown()
            .await
            .expect("the request is sent");
        drop(client);
        serving.await.expect("the server task is joined")
    });

    assert!(served.is_ok(), "{served:?}");
}

#[test]
fn connecting_to_a_socket_nobody_is_serving_names_the_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("absent.sock");
    let outcome = nanus_kernel::runtime::block_on(Client::connect(&socket));
    match outcome {
        Err(LinkError::Connect { path, .. }) => assert_eq!(path, socket),
        other => panic!("expected a refused connection, got {other:?}"),
    }
}

#[test]
fn a_socket_is_bound_for_its_owner_alone() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("agent.sock");
    nanus_kernel::runtime::block_on_local(async {
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        drop(listener);
    });

    let mode = std::fs::metadata(&socket)
        .expect("the socket exists")
        .permissions()
        .mode();
    // The agent can read the workspace and run programs as this user, so a socket any
    // local user could connect to would be a way to drive it. Owner-only is the floor.
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");
}

#[test]
fn a_socket_left_by_a_dead_process_is_replaced() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("agent.sock");
    nanus_kernel::runtime::block_on_local(async {
        // Bind twice at the same path with the first listener gone, which is exactly the
        // state a crashed agent leaves behind: a socket file nothing is listening on.
        let first = nanus_link::bind(&socket).await.expect("the first bind");
        drop(first);
        let second = nanus_link::bind(&socket).await;
        assert!(second.is_ok(), "a stale socket is replaced: {second:?}");
    });
}
