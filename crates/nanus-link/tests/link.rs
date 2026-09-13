//! The link, end to end: a real socket, a real turn, real sessions on disk.
//!
//! Everything here runs over `bind`ed sockets in a temporary directory rather than a
//! connected pair, because the parts most likely to be wrong are the ones a pair would
//! skip: the handshake, the attachment, the framing across process boundaries, the
//! permissions on the socket file, and the recording that has to have happened by the
//! time a client is told the turn is over.
//!
//! The model is scripted, so the suite needs no credential and no network. The point is
//! the transport and the sessions, not the provider.

#![cfg(feature = "server")]
// A panic in a test *is* the assertion, and a fixture with no sane default has nowhere
// else to put the failure. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::Path;
use std::rc::Rc;

// For `chain`, which is how the slow model delays its answer.
use futures::StreamExt as _;

use nanus_adapter_local::SystemClock;
use nanus_adapter_store::JsonlStore;
use nanus_bundle::AgentRunner;
use nanus_domain::{AgentConfig, Session, SessionEvent, SessionId, ToolRegistry};
use nanus_link::protocol::{Frame, Request};
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

/// A model that answers, but not immediately, so a test can catch a turn in flight.
struct SlowLlm;

impl LlmPort for SlowLlm {
    fn model(&self) -> &'static str {
        "slow"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(
            futures::stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                LlmEvent::TextDelta("late".to_owned())
            })
            .chain(futures::stream::iter(vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }])),
        )
    }
}

/// Builds an agent over a store in `dir`, and returns the store alongside it.
fn scripted_agent(dir: &Path) -> (Agent, StoreHandle) {
    agent_over(dir, Rc::new(Box::new(ScriptedLlm)), "scripted")
}

/// Builds an agent whose model is `llm`.
fn agent_over(dir: &Path, llm: Rc<Box<dyn LlmPort>>, model: &str) -> (Agent, StoreHandle) {
    let store = nanus_kernel::runtime::block_on(async {
        JsonlStore::new(dir.to_path_buf())
            .await
            .expect("the store opens")
            .handle()
    });
    let config = AgentConfig::new(4, 1, model, 4096).expect("a valid agent config");
    let runner = AgentRunner::new(llm, Rc::new(ToolRegistry::new()), "you are a test", config)
        .expect("a valid runner");
    let agent = Agent::from_parts(Parts {
        runner: Rc::new(runner),
        store: store.clone(),
        clock: SystemClock::new().handle(),
        workspace: dir.to_path_buf(),
        model: model.to_owned(),
        tools: 0,
    });
    (agent, store)
}

/// Serves `agent` on an already-bound `listener` until `stop` resolves.
///
/// The bind is done by the caller, before this is called, so that a test which connects
/// next cannot race the listener into existence.
fn serve(
    listener: tokio::net::UnixListener,
    agent: Agent,
    stop: impl Future<Output = ()> + 'static,
) -> tokio::task::JoinHandle<Result<(), LinkError>> {
    tokio::task::spawn_local(nanus_link::serve(listener, Rc::new(agent), stop))
}

/// Reads frames until a turn ends.
async fn turn_frames(client: &mut Client) -> Vec<Frame> {
    let mut frames = Vec::new();
    while let Some(frame) = client.next().await.expect("frames are readable") {
        let last = frame.is_end_of_turn();
        frames.push(frame);
        if last {
            break;
        }
    }
    frames
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

/// The answer a stream ended with, if it ended well.
fn answer_of(frames: &[Frame]) -> Option<&str> {
    frames.iter().find_map(|frame| match frame {
        Frame::Done { answer } => Some(answer.as_str()),
        _ => None,
    })
}

#[test]
fn a_prompt_streams_an_answer_and_records_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (info, attached, frames) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let info = client.info().clone();
        let attached = client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "say hello".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let frames = turn_frames(&mut client).await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        (info, attached, frames)
    });

    assert_eq!(info.model, "scripted");
    assert_eq!(info.tools, 0, "the scripted agent offers no tools");
    assert!(!attached.session.is_empty(), "the session is named");
    assert_eq!(attached.name, None, "an unnamed session has no name");
    assert_eq!(text_of(&frames), "hello back");
    assert_eq!(answer_of(&frames), Some("hello back"));

    // The contract the server keeps: by the time a client has seen the ending, the
    // session is already written down.
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1, "one new session");
    assert!(
        listed.first().is_some_and(|row| row.event_count > 0),
        "the turn left events behind: {listed:?}"
    );
}

#[test]
fn a_named_session_is_stored_and_can_be_attached_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (first, second) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        // A named session is written down at once, so the name is a promise the store
        // can keep even before anything is said in it.
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let first = client
            .start(Some("the-glob-bug".to_owned()))
            .await
            .expect("a named session starts");
        drop(client);

        // A second connection reaches the same session by name, and joins it rather
        // than loading a stale copy: the agent is already holding it.
        let mut resumed = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let second = resumed.attach("the-glob-bug").await.expect("attaches");

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (first, second)
    });

    assert_eq!(first.session, second.session, "the same session, by name");
    assert_eq!(second.name.as_deref(), Some("the-glob-bug"));
    assert_eq!(second.viewers, 1, "only the second client is attached now");

    // And the name is durable: it is in the store, not only in the agent.
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|row| row.name.clone()),
        Some("the-glob-bug".to_owned())
    );
    assert_eq!(
        nanus_kernel::runtime::block_on(store.resolve("the-glob-bug")).expect("resolve"),
        Some(SessionId::new(&first.session))
    );
}

#[test]
fn a_name_another_session_holds_is_refused_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let refused = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let _ = client
            .start(Some("mine".to_owned()))
            .await
            .expect("the first name is free");
        let refused = client.start(Some("mine".to_owned())).await;
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        refused
    });

    match refused {
        Err(LinkError::Agent(message)) => assert!(message.contains("mine"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The negative half that matters: a refused name must not leave an orphan session
    // behind, or every typo would add a conversation to the store.
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(
        listed.len(),
        1,
        "only the accepted session exists: {listed:?}"
    );
}

#[test]
fn a_running_session_can_be_listed_and_joined() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (held, owner_frames, watcher_frames) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        let mut owner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let mine = owner.start(None).await.expect("a session starts");

        let mut watcher = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let held = watcher.sessions().await.expect("the agent lists");

        // Two clients on one session: the owner asks, both see it.
        watcher.attach(&mine.session).await.expect("joins");
        owner
            .send(&Request::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let owner_frames = turn_frames(&mut owner).await;
        let watcher_frames = turn_frames(&mut watcher).await;

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (held, owner_frames, watcher_frames)
    });

    let listed = held.first().expect("the session is listed");
    assert_eq!(
        listed.viewers, 1,
        "only the owner is attached at that point"
    );
    assert!(!listed.busy);
    // The point of the feature: a turn runs once and both views show it.
    assert_eq!(text_of(&owner_frames), "hello back");
    assert_eq!(text_of(&watcher_frames), "hello back");
    assert_eq!(answer_of(&watcher_frames), Some("hello back"));
}

#[test]
fn a_turn_finishes_after_the_client_that_asked_for_it_leaves() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let attached = client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        // Gone before the turn can possibly have finished. The work belongs to the
        // session, not to the terminal that asked for it.
        drop(client);

        // A second client joins the same session and waits for the ending, which is the
        // proof the turn was still running after the first one left.
        let mut rejoined = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let same = rejoined
            .attach(&attached.session)
            .await
            .expect("reattaches");
        assert_eq!(same.session, attached.session);
        let frames = turn_frames(&mut rejoined).await;
        assert_eq!(answer_of(&frames), Some("hello back"));

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
    });

    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1);
    assert!(
        listed.first().is_some_and(|row| row.event_count > 0),
        "the abandoned turn was still recorded: {listed:?}"
    );
}

#[test]
fn a_session_that_was_never_held_is_loaded_from_the_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    // A session that predates this agent, as a restarted service would find.
    let saved = nanus_kernel::runtime::block_on(async {
        let mut session = Session::new(SessionId::new("earlier"), 1, "/work");
        session.append(SessionEvent::UserMessage {
            text: "from a previous life".to_owned(),
        });
        store.save(&session).await.expect("save");
        store.name(session.id(), "yesterday").await.expect("name");
        session.id().as_str().to_owned()
    });

    let attached = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let attached = client.attach("yesterday").await.expect("loads by name");
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        attached
    });

    assert_eq!(attached.session, saved);
    assert_eq!(attached.events, 1, "the stored log came with it");
}

#[test]
fn attaching_to_something_that_does_not_exist_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let refused = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let refused = client.attach("nothing-by-this-name").await;
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        refused
    });

    match refused {
        Err(LinkError::Agent(message)) => {
            assert!(message.contains("nothing-by-this-name"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_prompt_without_an_attachment_is_refused() {
    // A connection is a view of a session, and a view of nothing cannot run a turn.
    // Refusing beats guessing which conversation the client meant.
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frame = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client
            .send(&Request::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let frame = client.next().await.expect("a frame");
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        frame
    });

    match frame {
        Some(Frame::Failed { message }) => assert!(message.contains("attached"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_second_prompt_while_a_turn_runs_is_refused() {
    // One turn at a time, because a turn owns the session's log. Queueing the second
    // would be a promise about ordering the agent cannot keep: the model has not seen
    // the first answer yet.
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(dir.path(), Rc::new(Box::new(SlowLlm)), "slow");
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let seen = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        for text in ["first", "second"] {
            client
                .send(&Request::Prompt {
                    text: text.to_owned(),
                })
                .await
                .expect("the prompt is sent");
            // Long enough for the first turn to have started, far short of the model's
            // own delay.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut seen = Vec::new();
        while let Some(frame) = client.next().await.expect("frames are readable") {
            let last = frame.is_end_of_turn();
            seen.push(frame);
            if last {
                break;
            }
        }
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        seen
    });

    assert!(
        seen.iter().any(|frame| matches!(
            frame,
            Frame::Failed { message } if message.contains("already running")
        )),
        "the second prompt is refused while the first runs: {seen:?}"
    );
}

#[test]
fn a_status_or_listing_request_opens_no_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (described, held) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let described = client.ask_status().await.expect("a status reply");
        let held = client.sessions().await.expect("a listing");
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (described, held)
    });

    assert_eq!(described.model, "scripted");
    // Asking a question is not a conversation: nothing is held and nothing is stored,
    // which is the difference between this and a connection that used to *be* a session.
    assert!(held.is_empty(), "nothing is held: {held:?}");
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert!(listed.is_empty(), "nothing was created: {listed:?}");
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
        let serving = serve(listener, agent, std::future::pending());
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
