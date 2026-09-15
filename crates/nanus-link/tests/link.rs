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
use nanus_domain::{
    AgentConfig, Session, SessionEvent, SessionId, ToolCallId, ToolName, ToolRegistry, Usage,
};
use nanus_link::protocol::{Frame, Request, SessionInfo, TurnEnd};
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

/// A model that reports what its request cost, and takes long enough for the report to be
/// measured.
///
/// The first step generates *nothing but a tool call*, which is the case that reached no
/// listener at all before there was a callback for it. Its arguments arrive in two chunks with a
/// wait between them, so the generation window has two ends to be measured between: a usage
/// frame carrying a non-zero window is proof that the tool-call delta reached the clock, rather
/// than merely that the clock works.
struct MeteredLlm {
    /// The step the model is on, so the second request answers instead of calling again.
    step: std::cell::Cell<u32>,
}

impl LlmPort for MeteredLlm {
    fn model(&self) -> &'static str {
        "metered"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        let step = self.step.get();
        self.step.set(step.saturating_add(1));
        if step > 0 {
            return Box::pin(futures::stream::iter(vec![
                LlmEvent::TextDelta("finished".to_owned()),
                LlmEvent::Usage(Usage::new(30, 2, 0, 28, 2)),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]));
        }
        let opening = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call_1")),
                name: Some(ToolName::new("nowhere").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{".to_owned(),
            }
        });
        let closing = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            LlmEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "}".to_owned(),
            }
        });
        Box::pin(opening.chain(closing).chain(futures::stream::iter(vec![
            LlmEvent::Usage(Usage::new(20, 5, 3, 18, 2)),
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ])))
    }
}

/// A model that never stops asking for a tool, so a turn runs until something stops it.
///
/// The registry it is given is empty, which is enough: a call to a tool that is not
/// registered comes back as a failed *result*, which is the model's information rather
/// than a broken loop, so the turn keeps taking steps until the budget ends it.
struct RelentlessLlm;

impl LlmPort for RelentlessLlm {
    fn model(&self) -> &'static str {
        "relentless"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::iter(vec![
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call_1")),
                name: Some(ToolName::new("nowhere").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{}".to_owned(),
            },
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]))
    }
}

/// A model that asks for one tool, with arguments, and then answers.
///
/// The arguments are the point of this double. A call's *name* was all the link used to
/// carry, and a name is not enough to draw a call: an interface that cannot say which file
/// a `read` is reading can only say that something is happening.
struct OneToolLlm {
    /// The step the model is on, so the second request answers instead of calling again.
    step: std::cell::Cell<u32>,
}

impl LlmPort for OneToolLlm {
    fn model(&self) -> &'static str {
        "one-tool"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        let step = self.step.get();
        self.step.set(step.saturating_add(1));
        if step > 0 {
            return Box::pin(futures::stream::iter(vec![
                LlmEvent::TextDelta("finished".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]));
        }
        Box::pin(futures::stream::iter(vec![
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call_1")),
                name: Some(ToolName::new("nowhere").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: r#"{"file_path":"src/main.rs"}"#.to_owned(),
            },
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]))
    }
}

/// The same, but slow enough that a client can act while the turn is genuinely in flight.
///
/// A scripted stream that never awaits completes in one poll, so a test that waited for a
/// frame before interrupting would find the turn already over — which is a fact about the
/// script rather than about the interrupt.
struct SlowRelentlessLlm;

impl LlmPort for SlowRelentlessLlm {
    fn model(&self) -> &'static str {
        "slow-relentless"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(
            futures::stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("call_1")),
                    name: Some(ToolName::new("nowhere").unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: "{}".to_owned(),
                }
            })
            .chain(futures::stream::iter(vec![LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
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
        Frame::Done { answer, .. } => Some(answer.as_str()),
        _ => None,
    })
}

/// Why a stream's turn ended, when it ended with an outcome rather than a failure.
fn reason_of(frames: &[Frame]) -> Option<&TurnEnd> {
    frames.iter().find_map(|frame| match frame {
        Frame::Done { reason, .. } => Some(reason),
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
    assert_eq!(
        reason_of(&frames),
        Some(&TurnEnd::Completed),
        "a turn that answered says so"
    );

    // The contract the server keeps: by the time a client has seen the ending, the
    // session is already written down.
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1, "one new session");
    assert!(
        listed.first().is_some_and(|row| row.event_count > 0),
        "the turn left events behind: {listed:?}"
    );
}

/// The whole point of the request: a turn that would otherwise run to its budget stops
/// when a client asks it to, and everyone watching is told why it stopped.
///
/// `RelentlessLlm` never stops asking for tools, so without the interrupt this turn ends
/// at the step budget — which is what makes the assertion about the *reason* meaningful.
#[test]
fn an_interrupt_stops_the_turn_that_is_running() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(
        dir.path(),
        Rc::new(Box::new(SlowRelentlessLlm)),
        "slow-relentless",
    );
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frames = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "keep going".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        // A step has to be *running* for there to be something to stop, and the turn's
        // frames are the only evidence that it is.
        let mut frames = Vec::new();
        while let Some(frame) = client.next().await.expect("frames are readable") {
            let started = matches!(frame, Frame::Step { .. });
            frames.push(frame);
            if started {
                break;
            }
        }
        client
            .send(&Request::Interrupt)
            .await
            .expect("the interrupt is sent");
        frames.extend(turn_frames(&mut client).await);
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        frames
    });

    assert_eq!(
        reason_of(&frames),
        Some(&TurnEnd::Interrupted),
        "the turn stopped because it was asked to, not because it ran out: {frames:?}"
    );
    let steps = frames
        .iter()
        .filter(|frame| matches!(frame, Frame::Step { .. }))
        .count();
    assert_eq!(steps, 1, "and it stopped in the step it was in");
}

/// An interrupt with nothing to interrupt is not an error: it is a client that pressed the
/// key a moment after the turn ended, and the session is left exactly as it was.
#[test]
fn an_interrupt_with_no_turn_running_changes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frames = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        client
            .send(&Request::Interrupt)
            .await
            .expect("the interrupt is sent");
        // The next request still works, which is what "nothing happened" means from the
        // client's side: a refused interrupt would have ended the connection.
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
        frames
    });

    assert_eq!(reason_of(&frames), Some(&TurnEnd::Completed));
    let listed = nanus_kernel::runtime::block_on(store.list()).expect("the store lists");
    assert_eq!(listed.len(), 1, "the session is untouched");
}

/// The defect this closes: a turn that closed at its step budget reached the interface
/// looking exactly like one that had finished, because the ending said only that the turn
/// was over. `nanus run` had always called that a failed run; the link had no way to say
/// it at all.
#[test]
fn a_turn_that_runs_out_of_steps_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(dir.path(), Rc::new(Box::new(RelentlessLlm)), "relentless");
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (frames, steps) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "keep going".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let frames = turn_frames(&mut client).await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        let steps = frames
            .iter()
            .filter(|frame| matches!(frame, Frame::Step { .. }))
            .count();
        (frames, steps)
    });

    assert_eq!(
        reason_of(&frames),
        Some(&TurnEnd::MaxSteps),
        "the ending says why the turn stopped, not just that it did"
    );
    assert_eq!(
        steps, 4,
        "it stopped at the budget the agent was built with, so the reason is about the \
         budget rather than about the model"
    );
}

/// The defect this closes: a tool frame carried the tool's *name* and nothing else, so an
/// interface could say that something was running but never what — `read` of which file,
/// `bash` running which command. The arguments are in the session log, but a client
/// watching a turn is not reading the log as it is written, so the frame is where they have
/// to cross.
#[test]
fn a_tool_frame_carries_what_the_call_is_acting_on() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(
        dir.path(),
        Rc::new(Box::new(OneToolLlm {
            step: std::cell::Cell::new(0),
        })),
        "one-tool",
    );
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frames = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "look at the file".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let frames = turn_frames(&mut client).await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        frames
    });

    let call = frames.iter().find_map(|frame| match frame {
        Frame::Tool { name, arguments } => Some((name.as_str(), arguments)),
        _ => None,
    });
    let (name, arguments) = call.expect("the call reaches the client before it runs");
    assert_eq!(name, "nowhere");
    assert_eq!(
        arguments
            .get("file_path")
            .and_then(serde_json::Value::as_str),
        Some("src/main.rs"),
        "the arguments travel with the call: {arguments}"
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

/// A prompt refused because a turn is already running must not cancel an interrupt aimed at
/// that turn.
///
/// It used to: `start_turn` cleared the stop flag *before* checking whether it was starting
/// anything, so the refusal path cleared the running turn's flag instead of its own. One
/// terminal is enough to reach it — press Esc to stop, then press Enter with text in the
/// composer — and the turn then carried on to its budget.
#[test]
fn a_refused_prompt_does_not_cancel_an_interrupt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(
        dir.path(),
        Rc::new(Box::new(SlowRelentlessLlm)),
        "slow-relentless",
    );
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frames = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "keep going".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let mut frames = Vec::new();
        while let Some(frame) = client.next().await.expect("frames are readable") {
            let started = matches!(frame, Frame::Step { .. });
            frames.push(frame);
            if started {
                break;
            }
        }
        client
            .send(&Request::Interrupt)
            .await
            .expect("the interrupt is sent");
        // Refused, because the turn above is still running — and this is the request that
        // used to take the interrupt with it.
        client
            .send(&Request::Prompt {
                text: "me too".to_owned(),
            })
            .await
            .expect("the second prompt is sent");
        // Read on to the turn's own ending, skipping the refusal, which is a `Failed` frame
        // and would otherwise look like the end.
        while let Some(frame) = client.next().await.expect("frames are readable") {
            let ended = matches!(frame, Frame::Done { .. });
            frames.push(frame);
            if ended {
                break;
            }
        }
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        frames
    });

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::Failed { .. })),
        "the second prompt was refused: {frames:?}"
    );
    assert_eq!(
        reason_of(&frames),
        Some(&TurnEnd::Interrupted),
        "and the interrupt survived the refusal: {frames:?}"
    );
}

/// One session, one holder — even when two connections open it at the same moment.
///
/// `open_reference` checks whether a session is held, *awaits* the store, and only then
/// inserts. Two connections interleaving at those awaits used to end up with a `Held` each:
/// `busy` gated neither copy, both could run turns on stale copies, both recorded to the same
/// key, and the loser's client was invisible to `sessions`. The observable here is that a
/// prompt from one client reaches the other, which is only true if they watch one session.
#[test]
fn two_clients_opening_the_same_session_at_once_share_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let id = nanus_kernel::runtime::block_on(async {
        let session = Session::new(SessionId::new("shared"), 1, "/work");
        store.save(&session).await.expect("save");
        session.id().as_str().to_owned()
    });

    let (held, saw_the_prompt) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut a = Client::connect(&socket_for_client)
            .await
            .expect("the first client connects");
        let mut b = Client::connect(&socket_for_client)
            .await
            .expect("the second client connects");
        // Concurrently, so the two connections interleave at the store's awaits.
        let (first, second) = tokio::join!(a.attach(&id), b.attach(&id));
        assert!(first.is_ok() && second.is_ok(), "both attach");

        a.send(&Request::Prompt {
            text: "hello from a".to_owned(),
        })
        .await
        .expect("the prompt is sent");
        let mut saw_the_prompt = false;
        while let Some(frame) = b.next().await.expect("frames are readable") {
            if matches!(&frame, Frame::User { text } if text == "hello from a") {
                saw_the_prompt = true;
                break;
            }
            if frame.is_end_of_turn() {
                break;
            }
        }

        b.send(&Request::Sessions).await.expect("the listing");
        let mut held = 0;
        while let Some(frame) = b.next().await.expect("frames are readable") {
            if let Frame::Sessions { held: list } = frame {
                held = list.len();
                break;
            }
        }
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (held, saw_the_prompt)
    });

    assert!(
        saw_the_prompt,
        "the other client was told about the prompt, so both watch one session"
    );
    assert_eq!(held, 1, "and the agent holds it once, not twice");
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
fn re_attaching_leaves_no_stale_viewer_behind() {
    // The bug this pins: attaching again dropped the old `(viewer, session)` pair without
    // unsubscribing. The session being left kept queueing its frames into a client that
    // was watching something else, and — because it still counted as attached — could
    // never be let go, so an idle conversation was pinned in memory for the life of the
    // agent. Both ways of moving (`new` and `attach`) had it, so both are exercised.
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (first, second, held) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");

        // Move by `new`: two conversations on one connection.
        let first = client.start(None).await.expect("a session starts");
        let second = client.start(None).await.expect("a second session starts");
        let after_new = client.sessions().await.expect("the agent lists");

        // Move back by `attach`: the other way of leaving a session.
        let _back = client.attach(&first.session).await.expect("it reattaches");
        let after_attach = client.sessions().await.expect("the agent lists");

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (first, second, (after_new, after_attach))
    });

    let (after_new, after_attach) = held;
    let viewers_of = |listing: &[SessionInfo], id: &str| {
        listing
            .iter()
            .find(|session| session.session == id)
            .map_or_else(
                || panic!("{id} is not held: {listing:?}"),
                |found| found.viewers,
            )
    };

    // Leaving by `new`: the session left behind has nobody attached.
    assert_eq!(
        viewers_of(&after_new, &first.session),
        0,
        "the first session was left: {after_new:?}"
    );
    assert_eq!(viewers_of(&after_new, &second.session), 1);

    // Leaving by `attach`: the same, and the one gone back to has exactly one view.
    assert_eq!(
        viewers_of(&after_attach, &second.session),
        0,
        "the second session was left: {after_attach:?}"
    );
    assert_eq!(viewers_of(&after_attach, &first.session), 1);
}

#[test]
fn the_session_being_opened_is_never_the_one_let_go() {
    // The bug this pins: room was made *after* the newcomer was in the map, so a session
    // opened while every other one was in use evicted itself. The client was then handed
    // a conversation the agent no longer held — invisible to a listing, unreachable by a
    // second client, and loaded a second time by anyone who tried.
    //
    // Reaching it takes every other held session being in use, because otherwise the
    // least recently used *idle* one is evicted first and the newcomer is never a
    // candidate. So each of them has a client attached.
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let (last, held) = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        let mut clients = Vec::new();
        let mut last = String::new();
        for _ in 0..=nanus_link::server::MAX_HELD_SESSIONS {
            let mut client = Client::connect(&socket_for_client)
                .await
                .expect("the agent answers");
            last = client.start(None).await.expect("a session starts").session;
            // Held open: a session with a client attached is never an eviction candidate.
            clients.push(client);
        }
        let held = clients
            .first_mut()
            .expect("there is at least one client")
            .sessions()
            .await
            .expect("the agent lists");

        let _ = stop_tx.send(());
        drop(clients);
        serving.await.expect("joined").expect("clean");
        (last, held)
    });

    assert!(
        held.iter().any(|session| session.session == last),
        "the session just opened is still held: {last} not in {} sessions",
        held.len()
    );
    // The bound yields to the work rather than dropping a conversation somebody is in.
    assert!(
        held.len() > nanus_link::server::MAX_HELD_SESSIONS,
        "every session is in use, so none could be let go: {} held",
        held.len()
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

/// A usage frame reports the wait and the generation separately, and both are measured from the
/// deltas a *tool-call* step produces.
///
/// The two bounds are the point. A generation window wider than nothing says the tool call's own
/// deltas reached the clock — the one thing that could not be asserted before there was a
/// callback for them, because a step that answers with a call and no prose generates without
/// touching either of the callbacks that existed. A wait wider than nothing says the clock
/// started when the request was issued rather than at the first token, which is what makes the
/// wait a figure at all. And the parts have to fit inside the request they were taken from.
#[test]
fn a_usage_frame_separates_the_wait_from_the_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let metered = MeteredLlm {
        step: std::cell::Cell::new(0),
    };
    let (agent, _store) = agent_over(dir.path(), Rc::new(Box::new(metered)), "metered");
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let frames = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let _ = client.start(None).await.expect("a session starts");
        client
            .send(&Request::Prompt {
                text: "do something".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let frames = turn_frames(&mut client).await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        frames
    });

    let reported = frames.iter().find_map(|frame| match frame {
        Frame::Usage {
            ttft_ms,
            decode_ms,
            duration_ms,
            completion_tokens,
            reasoning_tokens,
            ..
        } => Some((
            *ttft_ms,
            *decode_ms,
            *duration_ms,
            *completion_tokens,
            *reasoning_tokens,
        )),
        _ => None,
    });
    let Some((ttft_ms, decode_ms, duration_ms, completion_tokens, reasoning_tokens)) = reported
    else {
        panic!("the turn reported usage: {frames:?}");
    };

    assert_eq!(completion_tokens, 5, "the model's own count travels");
    assert_eq!(
        reasoning_tokens, 3,
        "and thinking is reported beside it, inside it rather than added to it"
    );
    assert!(
        decode_ms > 0,
        "a tool call whose arguments took 80ms to finish generated for longer than no time: \
         {frames:?}"
    );
    assert!(
        ttft_ms > 0,
        "the wait began when the request was issued, not when the first token arrived"
    );
    assert!(
        ttft_ms.saturating_add(decode_ms) <= duration_ms,
        "the parts fit inside the request: {ttft_ms} + {decode_ms} > {duration_ms}"
    );
}
