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
use std::sync::Arc;

// For `chain`, which is how the slow model delays its answer.
use futures::StreamExt as _;

use nanus_adapter_local::SystemClock;
use nanus_adapter_store::JsonlStore;
use nanus_bundle::AgentRunner;
use nanus_domain::{
    AgentConfig, ApprovalPolicy, SandboxMode, Session, SessionEvent, SessionId, ToolAccess,
    ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolRegistry,
    ToolResult, ToolSchema, Usage,
};
use nanus_link::protocol::{ApprovalState, EffortState, Frame, Request, SessionInfo, TurnEnd};
use nanus_link::server::{Agent, Parts};
use nanus_link::{Client, LinkError};
// `StorePort` is in scope for the concrete store the claim test writes through: `save` and
// `name` live on the port, and `lock_file` on the adapter.
use nanus_ports::{
    ChatRequest, FinishReason, LlmEvent, LlmPort, LlmStream, StoreHandle, StorePort as _,
};

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

/// A model that holds its turn open until the test lets it answer.
///
/// What makes "a client attaches in the middle of a turn" a fact rather than a race: the
/// test waits for the permit the stream takes on entry, and the turn is then inside the
/// model's response — with its steps already broadcast — until the release permit is given.
struct WaitingLlm {
    /// Given by the stream when it is entered, so the test knows the turn is in flight.
    entered: Arc<tokio::sync::Semaphore>,
    /// Awaited by the stream, so the turn stays in flight until the test says otherwise.
    release: Arc<tokio::sync::Semaphore>,
}

impl LlmPort for WaitingLlm {
    fn model(&self) -> &'static str {
        "waiting"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        let held = futures::stream::once(async move {
            entered.add_permits(1);
            let _permit = release.acquire().await;
            LlmEvent::TextDelta("the rest".to_owned())
        });
        Box::pin(held.chain(futures::stream::iter(vec![LlmEvent::Finished {
            reason: FinishReason::Stop,
        }])))
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
        let acknowledged = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            LlmEvent::ResponseHead
        });
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
        Box::pin(
            acknowledged
                .chain(opening)
                .chain(closing)
                .chain(futures::stream::iter(vec![
                    LlmEvent::Usage(Usage::new(20, 5, 3, 18, 2)),
                    LlmEvent::Finished {
                        reason: FinishReason::ToolCalls,
                    },
                ])),
        )
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
    let runner = AgentRunner::new(
        llm,
        nanus_bundle::ToolRegistryHandle::new(ToolRegistry::new()),
        "you are a test",
        config,
    )
    .expect("a valid runner");
    let agent = Agent::from_parts(Parts {
        runner: Rc::new(runner),
        store: store.clone(),
        clock: SystemClock::new().handle(),
        workspace: dir.to_path_buf(),
        // The default provider's ids plus whatever this test scripts, so a switch has
        // somewhere to go and the model in use is still in the list. The names live in
        // the bundle's provider table, which is the only place that knows them.
        models: nanus_bundle::provider::DEFAULT_PROVIDER
            .models()
            .iter()
            .map(|id| (*id).to_owned())
            .collect(),
        tools: 0,
    });
    (agent, store)
}

/// A model that calls the `runner` tool once and then answers.
///
/// The step counter matters: the second request is answered rather than calling again, so a
/// turn ends. `Cell` because the port is shared by reference and never needs to be `Send`.
struct CallingLlm {
    step: std::cell::Cell<u32>,
}

impl LlmPort for CallingLlm {
    fn model(&self) -> &'static str {
        "calling"
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
                name: Some(ToolName::new("runner").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{}".to_owned(),
            },
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]))
    }
}

/// A tool that succeeds without touching anything, declared as running a program.
///
/// `Execute` is the access no confined sandbox permits, which is what makes every call to it
/// need an exception and therefore a decision.
struct StubTool;

impl ToolExecutor for StubTool {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        Box::pin(async move { ToolResult::success(call.id, serde_json::json!({ "ran": true })) })
    }
}

/// Builds an agent with one gated tool and the given approval policy.
///
/// The sandbox is `workspace_write`, which does not permit a program, so the call needs an
/// exception: with `ask` the turn puts the question to a watching client, and with `never`
/// it refuses the call outright.
fn gated_agent(dir: &Path, approval: ApprovalPolicy) -> (Agent, StoreHandle) {
    let store = nanus_kernel::runtime::block_on(async {
        JsonlStore::new(dir.to_path_buf())
            .await
            .expect("the store opens")
            .handle()
    });
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: ToolName::new("runner").unwrap_or_else(|_| unreachable!("a valid tool name")),
        description: "A tool that runs something".to_owned(),
        parameters: serde_json::json!({ "type": "object" }),
    };
    let registered =
        registry.register(ToolDefinition::new(schema, StubTool).with_access(ToolAccess::Execute));
    assert!(registered.is_ok(), "the gated tool registers");
    let config = AgentConfig::new(4, 1, "calling", 4096)
        .expect("a valid agent config")
        .with_sandbox(SandboxMode::WorkspaceWrite)
        .with_approval(approval);
    let runner = AgentRunner::new(
        Rc::new(Box::new(CallingLlm {
            step: std::cell::Cell::new(0),
        })),
        nanus_bundle::ToolRegistryHandle::new(registry),
        "you are a test",
        config,
    )
    .expect("a valid runner");
    let agent = Agent::from_parts(Parts {
        runner: Rc::new(runner),
        store: store.clone(),
        clock: SystemClock::new().handle(),
        workspace: dir.to_path_buf(),
        models: vec![String::from("calling")],
        tools: 1,
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

/// A model switch reaches the agent, is broadcast to the client that asked, and is refused by
/// name when the id is one the agent does not offer.
///
/// The refusal is the half that matters: an id forwarded to the provider is a request rather
/// than a diagnosis, and the sentence names the ids that do exist so a client can act on it.
#[test]
fn a_model_switch_reaches_the_agent_and_is_answered() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(dir.path(), Rc::new(Box::new(ScriptedLlm)), "scripted");
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let outcome = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        // The handshake is where a client learns what it may ask for: an interface that
        // cycled a list of its own would offer models the agent refuses.
        let offered = client.info().models.clone();
        client.start(None).await.expect("a session starts");
        // The attachment says which model is answering, so the client draws it before the
        // reader types rather than after the first answer. The frames after `Attached` are the
        // approval state and this one.
        let before = loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::ModelChanged { model }) => break model,
                Some(_) => {}
                None => break String::new(),
            }
        };
        assert_eq!(
            before, "scripted",
            "the attachment says which model answers"
        );

        let chosen = String::from("deepseek-v4-pro");
        client
            .send(&Request::SetModel {
                model: chosen.clone(),
            })
            .await
            .expect("the switch is sent");
        let broadcast = loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::ModelChanged { model }) => break model,
                Some(_) => {}
                None => break String::new(),
            }
        };

        // An id the agent does not offer is refused, and the sentence says what it does offer.
        client
            .send(&Request::SetModel {
                model: String::from("deepseek-chat"),
            })
            .await
            .expect("the request is sent");
        let refused = loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::Failed { message }) => break message,
                Some(_) => {}
                None => break String::new(),
            }
        };
        let _ = stop_tx.send(());
        let _ = serving.await;
        (offered, broadcast, refused)
    });
    let (offered, broadcast, refused) = outcome;

    assert!(
        offered.contains(&String::from("deepseek-v4-pro")),
        "the handshake offers the models the agent will accept: {offered:?}"
    );
    assert_eq!(
        broadcast, "deepseek-v4-pro",
        "the switch is broadcast to the client that made it"
    );
    assert!(
        refused.contains("deepseek-chat") && refused.contains("deepseek-v4-pro"),
        "the refusal names the id and the ones that exist: {refused}"
    );
}

/// The effort a client asks for reaches the agent and is broadcast to everyone watching.
///
/// There is nothing to validate here unlike the model: the scale is the protocol's own, and the
/// only question is whether the choice arrives and whether the other clients hear about it.
#[test]
fn an_effort_switch_reaches_the_agent_and_is_broadcast() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = agent_over(dir.path(), Rc::new(Box::new(ScriptedLlm)), "scripted");
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    let state = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        // The stub adapter has no notion of effort, so the handshake says so and the attach
        // sends no effort frame: "no notion" is an absence rather than a fourth state.
        assert_eq!(client.info().effort, None);
        client.start(None).await.expect("a session starts");
        let attached = loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::ModelChanged { .. }) => break true,
                Some(_) => {}
                None => break false,
            }
        };
        assert!(attached, "the attach frames arrived");
        client
            .send(&Request::SetEffort {
                state: EffortState::High,
            })
            .await
            .expect("the request is sent");
        let state = loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::EffortChanged { state }) => break Some(state),
                Some(_) => {}
                None => break None,
            }
        };
        let _ = stop_tx.send(());
        let _ = serving.await;
        state
    });

    assert_eq!(
        state,
        Some(EffortState::High),
        "the effort the client asked for is the one the agent says it is using"
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
        Frame::Tool {
            call_id,
            name,
            arguments,
        } => Some((call_id.as_deref(), name.as_str(), arguments)),
        _ => None,
    });
    let (call_id, name, arguments) = call.expect("the call reaches the client before it runs");
    assert_eq!(name, "nowhere");
    assert_eq!(
        arguments
            .get("file_path")
            .and_then(serde_json::Value::as_str),
        Some("src/main.rs"),
        "the arguments travel with the call: {arguments}"
    );

    // The id travels too, and it is what pairs this frame with the `ToolDone` that answers
    // it: a step's calls all go out before any of its results, and a step's results go out in
    // the order the tools finished rather than the order they were asked for. Without the id
    // a watcher has only the name, which cannot tell two calls to one tool apart.
    let done = frames.iter().find_map(|frame| match frame {
        Frame::ToolDone { call_id, name, .. } => Some((call_id.as_deref(), name.as_str())),
        _ => None,
    });
    let (done_id, done_name) = done.expect("the call is answered");
    assert_eq!(done_name, "nowhere");
    assert_eq!(call_id, done_id, "the call and its result name the same id");
    assert!(
        call_id.is_some_and(|id| !id.is_empty()),
        "and the agent sent one rather than leaving the client to guess"
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
        // Another case is the same name: the agent resolves a name through the store, so
        // `MINE` finds the session called `mine` and is refused rather than starting a second
        // conversation a reader would believe was the first.
        let cased = client.start(Some("MINE".to_owned())).await;
        // And the same word, typed in another case, attaches to it rather than being refused as
        // unknown.
        let attached = client.attach("MiNe").await;
        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        (refused, cased, attached)
    });

    let (refused, cased, attached) = refused;
    for refusal in [refused, cased] {
        match refusal {
            // The sentence echoes the name as it was typed, so the check is on the word rather
            // than on its case.
            Err(LinkError::Agent(message)) => {
                assert!(message.to_lowercase().contains("mine"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    assert_eq!(
        attached.map(|info| info.name).ok().flatten().as_deref(),
        Some("mine"),
        "a name resolves whatever case it is typed in"
    );
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

/// A client that attaches while a turn is running is shown the whole turn, not its tail.
///
/// The turn's frames went to the clients that were there, and the store does not have the
/// turn yet — it is written when the turn ends — so a client arriving in the middle of one
/// has nothing to read. The agent therefore hands it the part of the running turn that the
/// log cannot: the prompt, the steps, and the deltas so far, in order, followed by the live
/// turn from there.
#[test]
fn a_client_that_attaches_mid_turn_catches_up() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (agent, _store) = agent_over(
        dir.path(),
        Rc::new(Box::new(WaitingLlm {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })),
        "waiting",
    );
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        let mut owner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let mine = owner.start(None).await.expect("a session starts");
        owner
            .send(&Request::Prompt {
                text: "do the thing".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        // The turn is now inside the model's response: the prompt and the step have been
        // broadcast, and nothing else has been produced.
        let _permit = entered.acquire().await.expect("the turn reached the model");

        let mut joiner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        joiner.attach(&mine.session).await.expect("joins");

        // The backlog is the first thing after the attachment, and it holds the turn so far.
        let first = joiner.next().await.expect("frames are readable");
        let Some(Frame::Backlog { frames }) = first else {
            panic!("expected the turn in flight, got {first:?}");
        };
        let mut asked = None;
        let mut stepped = None;
        let mut text = String::new();
        for frame in &frames {
            match frame {
                Frame::User { text } => asked = Some(text.clone()),
                Frame::Step { step } => stepped = Some(*step),
                Frame::Text { delta } => text.push_str(delta),
                _ => {}
            }
        }
        // The asking client never sees its own prompt on the wire, so this is the only
        // record a late attacher can be given of what the turn is answering.
        assert_eq!(asked.as_deref(), Some("do the thing"));
        assert_eq!(stepped, Some(1));
        assert!(text.is_empty(), "the model has not answered yet: {text:?}");

        // Released, the rest of the turn reaches both views, and the joiner's transcript
        // reads as one turn rather than as one beginning halfway through.
        release.add_permits(1);
        let rest = turn_frames(&mut joiner).await;
        assert_eq!(text_of(&rest), "the rest");
        assert_eq!(answer_of(&rest), Some("the rest"));
        let owner_frames = turn_frames(&mut owner).await;
        assert_eq!(text_of(&owner_frames), "the rest");

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
    });
}

/// A turn that has ended is not sent twice: a client attaching to an idle session is given
/// no backlog, because the store is where a finished turn lives.
#[test]
fn an_idle_session_catches_nobody_up() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        let mut owner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let mine = owner.start(None).await.expect("a session starts");
        owner
            .send(&Request::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let _ = turn_frames(&mut owner).await;

        let mut joiner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        joiner.attach(&mine.session).await.expect("joins");
        // Everything the joiner is sent after the attachment is one of the attachment's own
        // frames: the turn is over, so there is nothing to catch it up with.
        while let Some(frame) = joiner.next().await.expect("frames are readable") {
            assert!(
                !matches!(frame, Frame::Backlog { .. }),
                "an idle session has no turn in flight: {frame:?}"
            );
            if matches!(frame, Frame::ModelChanged { .. }) {
                break;
            }
        }

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
    });
}

/// The count an attachment carries is the log's, taken where the backlog begins.
///
/// A client reads the log itself, a moment after the agent snapshotted the running turn, and
/// compares the two: a log that has moved past the count already holds the turn the backlog
/// carries, and the backlog is dropped rather than drawn on top of it. That comparison is only
/// exact if the count is the *log's* own at the instant of the snapshot — not the running
/// turn's, which the log does not have yet — so the number is asserted here against the store
/// rather than inferred from the frames.
#[test]
fn an_attachment_reports_the_log_position_the_backlog_continues_from() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (agent, store) = agent_over(
        dir.path(),
        Rc::new(Box::new(WaitingLlm {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })),
        "waiting",
    );
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    // A session with history of its own, as a resumed conversation has: the log holds events
    // *before* the turn that will be running.
    let saved = SessionId::new("positioned");
    let held_turn = nanus_kernel::runtime::block_on(async {
        let mut session = Session::new(saved.clone(), 1, "/work");
        session.append(SessionEvent::UserMessage {
            text: "an earlier question".to_owned(),
        });
        session.append(SessionEvent::AssistantMessage {
            text: Some("an earlier answer".to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        store.save(&session).await.expect("the history is recorded");
        u64::try_from(session.event_count()).unwrap_or(u64::MAX)
    });

    nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });

        let mut owner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let attached = owner.attach(saved.as_str()).await.expect("resumes");
        assert_eq!(
            attached.events, held_turn,
            "an idle session reports what its log holds"
        );
        owner
            .send(&Request::Prompt {
                text: "and now this".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let _permit = entered.acquire().await.expect("the turn reached the model");

        let mut joiner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let mid_turn = joiner.attach(saved.as_str()).await.expect("joins");
        // The running turn is in the backlog and *not* in the count, which is what lets the
        // joiner tell "the log ends here" from "the log already has this turn".
        assert_eq!(
            mid_turn.events, held_turn,
            "a running turn does not move the log's position"
        );
        let stored = store
            .load(&saved)
            .await
            .expect("the history is readable")
            .event_count();
        assert_eq!(
            u64::try_from(stored).unwrap_or(u64::MAX),
            mid_turn.events,
            "the count is the log's own, so a client can compare against what it read"
        );

        release.add_permits(1);
        let rest = turn_frames(&mut joiner).await;
        assert!(answer_of(&rest).is_some(), "the turn ended: {rest:?}");

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
    });
}

/// A rename reaches everything the agent reports about a session it is holding.
///
/// `nanus sessions name` writes the store and tells nobody — it does not connect to the link —
/// so the agent's copy of the name is what goes stale. A listing is where that shows, and an
/// attachment labels the client's screen with the same word, so both refresh from the store
/// rather than trusting the cache. The other half matters more: a name is *resolved* through
/// the store too, so the name a session used to have cannot be used to join it.
#[test]
fn a_rename_reaches_a_held_session() {
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

        let mut owner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let mine = owner
            .start(Some("first-name".to_owned()))
            .await
            .expect("a session starts");
        assert_eq!(mine.name.as_deref(), Some("first-name"));
        assert!(!mine.busy);

        // Renamed behind the agent's back, exactly as the CLI does it.
        store
            .name(&SessionId::new(&mine.session), "second-name")
            .await
            .expect("the store records the name");

        // A listing shows what the session is called now.
        let listed = owner.sessions().await.expect("the agent lists");
        let entry = listed
            .iter()
            .find(|entry| entry.session == mine.session)
            .expect("the session is listed");
        assert_eq!(
            entry.name.as_deref(),
            Some("second-name"),
            "the listing refreshed the name"
        );

        // And so does the attachment that labels a client's screen.
        let mut joiner = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let attached = joiner.attach(&mine.session).await.expect("joins");
        assert_eq!(attached.name.as_deref(), Some("second-name"));

        // The name it used to have names nothing, and resolving through the store is what
        // makes that true: matching the agent's cache would have joined this session.
        let mut stale = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let refused = stale.attach("first-name").await;
        assert!(refused.is_err(), "the old name is gone: {refused:?}");

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
    });
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

/// A session another live agent is writing is refused, naming the holder.
///
/// The agent claims every session it holds, so this is what a second `nanus tui --resume`
/// meets — and what a `nanus run --resume` meets against a running service. Without the claim
/// the second agent loads its own copy and the two take turns overwriting each other's log,
/// with the loser's turn simply gone.
#[test]
fn a_session_another_agent_is_writing_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _handle) = scripted_agent(dir.path());
    let socket = dir.path().join("agent.sock");
    let socket_for_client = socket.clone();

    // The concrete store, rather than the handle the agent was built over, because the test
    // needs to write where a claim lives.
    let store = nanus_kernel::runtime::block_on(JsonlStore::new(dir.path().to_path_buf()))
        .expect("the store opens");

    // A session on disk that this agent is not holding, as a second agent would find it.
    let saved = nanus_kernel::runtime::block_on(async {
        let mut session = Session::new(SessionId::new("contested"), 1, "/work");
        session.append(SessionEvent::UserMessage {
            text: "nobody has this open yet".to_owned(),
        });
        store.save(&session).await.expect("save");
        store.name(session.id(), "shared-work").await.expect("name");
        session.id().clone()
    });

    // Another agent's claim. A store of its own takes the lock the operating system arbitrates,
    // which is what a second process's claim is — a hand-written file would not be one, because
    // the label is not the lock.
    let other = nanus_kernel::runtime::block_on(JsonlStore::new(dir.path().to_path_buf()))
        .expect("the second store opens");
    nanus_kernel::runtime::block_on(other.lock(&saved, "nanus at /tmp/other.sock"))
        .expect("the other agent holds it");

    let saved_after = saved.clone();
    let message = nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving = serve(listener, agent, async move {
            let _ = stop_rx.await;
        });
        let mut client = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        // Refused by name and by id: a name resolves to the id that is claimed.
        let by_name = client.attach("shared-work").await;
        let message = match by_name {
            Err(error) => error.to_string(),
            Ok(attached) => panic!("the claim must refuse this: {attached:?}"),
        };

        let mut by_id = Client::connect(&socket_for_client)
            .await
            .expect("the agent answers");
        let refused = by_id.attach(saved.as_str()).await;
        assert!(
            refused.is_err(),
            "a claimed session is refused: {refused:?}"
        );

        let _ = stop_tx.send(());
        serving.await.expect("joined").expect("clean");
        message
    });

    // The sentence names what is writing it and what to do instead, because "locked" on its own
    // leaves a reader with nowhere to go.
    assert!(message.contains("nanus at /tmp/other.sock"), "{message}");
    assert!(message.contains("--connect"), "{message}");
    // A refusal must not take the other agent's claim away: a third writer still cannot have it.
    let third = nanus_kernel::runtime::block_on(JsonlStore::new(dir.path().to_path_buf()))
        .expect("the third store opens");
    assert!(
        nanus_kernel::runtime::block_on(third.lock(&saved_after, "a third writer")).is_err(),
        "the other agent's claim survives the refusal"
    );
    nanus_kernel::runtime::block_on(other.lock(&saved_after, "nanus at /tmp/other.sock"))
        .expect("the holder re-claims its own");
    other.release_lock(&saved_after);
    nanus_kernel::runtime::block_on(third.lock(&saved_after, "a third writer"))
        .expect("released, the session is claimable");
    third.release_lock(&saved_after);
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

/// A usage frame reports the wait, how much of it went on being answered at all, and the
/// generation separately — and all of them are measured from the deltas a *tool-call* step
/// produces.
///
/// The two bounds are the point. A generation window wider than nothing says the tool call's own
/// deltas reached the clock — the one thing that could not be asserted before there was a
/// callback for them, because a step that answers with a call and no prose generates without
/// touching either of the callbacks that existed. A wait wider than nothing says the clock
/// started when the request was issued rather than at the first token, which is what makes the
/// wait a figure at all. A head wider than nothing, and narrower than the wait it divides, says the
/// split is real rather than a relabelling. And the parts have to fit inside the request they were
/// taken from.
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
            head_ms,
            ttft_ms,
            decode_ms,
            duration_ms,
            completion_tokens,
            reasoning_tokens,
            ..
        } => Some((
            *head_ms,
            *ttft_ms,
            *decode_ms,
            *duration_ms,
            *completion_tokens,
            *reasoning_tokens,
        )),
        _ => None,
    });
    let Some((head_ms, ttft_ms, decode_ms, duration_ms, completion_tokens, reasoning_tokens)) =
        reported
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
    assert!(
        head_ms > 0,
        "being answered at all is a wait, and it took 50ms: {frames:?}"
    );
    assert!(
        head_ms <= ttft_ms,
        "the server answered before it spoke: head {head_ms}ms, first token {ttft_ms}ms"
    );
    // The server's own share is the rest of the wait, and it has to hold the 120ms it spent before
    // its first delta — the half prefill lives in, which is the point of splitting the wait.
    assert!(
        ttft_ms.saturating_sub(head_ms) > 0,
        "the server worked after it answered: {ttft_ms} - {head_ms}"
    );
}

/// Reads frames until the agent asks to approve a call, and returns the question.
///
/// The question is what a watching client is waiting for, so the loop stops there rather
/// than at the end of the turn: an answer has to be sent while the turn is *blocked* on it.
async fn until_approval(client: &mut Client, frames: &mut Vec<Frame>) -> Option<(String, String)> {
    loop {
        let frame = client.next().await.expect("frames are readable")?;
        let question = match &frame {
            Frame::Approval { call_id, tool, .. } => Some((call_id.clone(), tool.clone())),
            _ => None,
        };
        let ended = frame.is_end_of_turn();
        frames.push(frame);
        if question.is_some() {
            return question;
        }
        if ended {
            return None;
        }
    }
}

/// The gate means something over the link: a call outside the sandbox is put to the client
/// watching, and its answer runs the call.
#[test]
fn an_approval_question_reaches_the_client_and_its_answer_runs_the_call() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = gated_agent(dir.path(), ApprovalPolicy::PerCall);
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
                text: "run it".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let mut frames = Vec::new();
        let question = until_approval(&mut client, &mut frames).await;
        let Some((call_id, tool)) = question else {
            panic!("the agent asked about the call: {frames:?}");
        };
        assert_eq!(tool, "runner", "the question names the tool");
        client
            .send(&Request::Approve {
                call_id,
                allow: true,
                always: false,
            })
            .await
            .expect("the answer is sent");
        frames.extend(turn_frames(&mut client).await);
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        frames
    });

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::ToolDone { error: false, .. })),
        "the approved call ran and succeeded: {frames:?}"
    );
    assert_eq!(answer_of(&frames), Some("finished"));
    assert_eq!(reason_of(&frames), Some(&TurnEnd::Completed));
}

/// The other direction: a refusal stops the call, is recorded as a failed result rather than
/// a harness error, and the turn still finishes — the model is told and can respond.
#[test]
fn a_refused_approval_denies_the_call_and_the_turn_finishes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = gated_agent(dir.path(), ApprovalPolicy::PerCall);
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
                text: "run it".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let mut frames = Vec::new();
        let question = until_approval(&mut client, &mut frames).await;
        let Some((call_id, _)) = question else {
            panic!("the agent asked about the call: {frames:?}");
        };
        client
            .send(&Request::Approve {
                call_id,
                allow: false,
                always: false,
            })
            .await
            .expect("the refusal is sent");
        frames.extend(turn_frames(&mut client).await);
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        frames
    });

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::ToolDone { error: true, .. })),
        "the refused call is reported as a failed result: {frames:?}"
    );
    assert_eq!(
        answer_of(&frames),
        Some("finished"),
        "the model was told and answered"
    );
    assert_eq!(reason_of(&frames), Some(&TurnEnd::Completed));
}

/// `all_calls` grants the call without asking anyone, so no question crosses the link.
#[test]
fn an_all_calls_state_grants_the_call_without_asking_the_client() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = gated_agent(dir.path(), ApprovalPolicy::AllCalls);
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
                text: "run it".to_owned(),
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

    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, Frame::Approval { .. })),
        "`all_calls` consults nobody: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::ToolDone { error: false, .. })),
        "the call ran: {frames:?}"
    );
    assert_eq!(reason_of(&frames), Some(&TurnEnd::Completed));
}

/// An `always` answer is a standing grant: the next turn does not ask about the same tool.
#[test]
fn an_always_answer_is_not_asked_again_in_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = gated_agent(dir.path(), ApprovalPolicy::PerCall);
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

        // The first turn asks, and the answer is a standing permission for the tool.
        client
            .send(&Request::Prompt {
                text: "run it".to_owned(),
            })
            .await
            .expect("the prompt is sent");
        let mut frames = Vec::new();
        let Some((call_id, _)) = until_approval(&mut client, &mut frames).await else {
            panic!("the first call is asked about: {frames:?}");
        };
        client
            .send(&Request::Approve {
                call_id,
                allow: true,
                always: true,
            })
            .await
            .expect("the standing answer is sent");
        frames.extend(turn_frames(&mut client).await);

        // The second turn calls the same tool, and nobody is asked this time.
        client
            .send(&Request::Prompt {
                text: "again".to_owned(),
            })
            .await
            .expect("the second prompt is sent");
        let second = turn_frames(&mut client).await;
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
        (frames, second)
    });

    let (first, second) = frames;
    assert!(
        first
            .iter()
            .any(|frame| matches!(frame, Frame::Approval { .. })),
        "the first call is a question: {first:?}"
    );
    assert!(
        !second
            .iter()
            .any(|frame| matches!(frame, Frame::Approval { .. })),
        "the granted tool is not asked about again: {second:?}"
    );
}

/// The interface can change the agent's state; the change is acknowledged, and the next call
/// is decided by the new state rather than the configured one.
#[test]
fn setting_the_approval_state_changes_how_the_next_call_is_decided() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (agent, _store) = gated_agent(dir.path(), ApprovalPolicy::PerCall);
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
            .send(&Request::SetApproval {
                state: ApprovalState::AllCalls,
            })
            .await
            .expect("the state is sent");
        client
            .send(&Request::Prompt {
                text: "run it".to_owned(),
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

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            Frame::ApprovalChanged {
                state: ApprovalState::AllCalls
            }
        )),
        "the change is acknowledged: {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, Frame::Approval { .. })),
        "the new state granted the call: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::ToolDone { error: false, .. })),
        "the call ran: {frames:?}"
    );
}
