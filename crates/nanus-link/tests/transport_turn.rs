//! One agent and durable turn across either platform's real transport.
#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::rc::Rc;

use nanus_adapter_local::SystemClock;
use nanus_adapter_store::JsonlStore;
use nanus_bundle::{AgentRunner, ToolRegistryHandle};
use nanus_domain::{AgentConfig, SessionId, ToolRegistry};
use nanus_link::{Agent, Parts};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, LlmPort, LlmStream};

use nanus_link::transport::bind;
use nanus_link::{Client, Frame, LinkError, Request};
use std::path::{Path, PathBuf};

fn endpoint(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        dir.join("agent.sock")
    }
    #[cfg(windows)]
    {
        let base = nanus_link::paths::attached_endpoint(dir, std::process::id()).expect("SID");
        PathBuf::from(format!("{}-{}", base.display(), uuid::Uuid::new_v4()))
    }
}

struct Scripted;

impl LlmPort for Scripted {
    fn model(&self) -> &'static str {
        "scripted"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::iter([
            LlmEvent::TextDelta(String::from("hello back")),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]))
    }
}

#[test]
fn shutdown_releases_the_endpoint_after_probes_and_an_idle_connection() {
    let dir = tempfile::tempdir().expect("store");
    nanus_kernel::runtime::block_on_local(async {
        let store = JsonlStore::new(dir.path().to_owned())
            .await
            .expect("store")
            .handle();
        let runner = AgentRunner::new(
            Rc::new(Box::new(Scripted)),
            ToolRegistryHandle::new(ToolRegistry::new()),
            "you are a test",
            AgentConfig::new(4, 1, "scripted", 4096).expect("config"),
            SystemClock::new().handle(),
        )
        .expect("runner");
        let agent = Rc::new(Agent::from_parts(Parts {
            runner: Rc::new(runner),
            store,
            clock: SystemClock::new().handle(),
            workspace: dir.path().to_owned(),
            models: vec![String::from("scripted")],
            tools: 0,
            switch: None,
        }));
        let endpoint = endpoint(dir.path());
        let listener = bind(&endpoint).await.expect("bind");
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, agent, std::future::pending()));
        for _ in 0..3 {
            drop(Client::connect(&endpoint).await.expect("probe"));
        }
        let idle = Client::connect(&endpoint).await.expect("idle connection");
        let mut stopping = Client::connect(&endpoint).await.expect("stop connection");
        stopping.request_shutdown().await.expect("stop");
        tokio::time::timeout(std::time::Duration::from_secs(5), serving)
            .await
            .expect("bounded shutdown")
            .expect("server task")
            .expect("clean shutdown");
        drop(idle);
        drop(stopping);
        // Unix leaves its socket path for the process owner to remove; pipes have no path.
        #[cfg(unix)]
        std::fs::remove_file(&endpoint).expect("remove socket");
        let rebound = bind(&endpoint).await.expect("the old owner is gone");
        drop(rebound);
    });
}

#[cfg(windows)]
#[test]
fn an_agent_drops_an_unproven_client_and_keeps_serving() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let dir = tempfile::tempdir().expect("store");
    nanus_kernel::runtime::block_on_local(async {
        let store = JsonlStore::new(dir.path().to_owned())
            .await
            .expect("store")
            .handle();
        let runner = AgentRunner::new(
            Rc::new(Box::new(Scripted)),
            ToolRegistryHandle::new(ToolRegistry::new()),
            "you are a test",
            AgentConfig::new(4, 1, "scripted", 4096).expect("config"),
            SystemClock::new().handle(),
        )
        .expect("runner");
        let agent = Rc::new(Agent::from_parts(Parts {
            runner: Rc::new(runner),
            store,
            clock: SystemClock::new().handle(),
            workspace: dir.path().to_owned(),
            models: vec![String::from("scripted")],
            tools: 0,
            switch: None,
        }));
        let endpoint = endpoint(dir.path());
        let listener = bind(&endpoint).await.expect("bind");
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, agent, std::future::pending()));
        let mut stranger = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&endpoint)
            .expect("the pipe opens");
        stranger
            .write_all(&[b'x'; 64])
            .await
            .expect("not a challenge");
        let mut received = Vec::new();
        let _ = stranger.read_to_end(&mut received).await;
        assert!(
            received.is_empty(),
            "the stranger was sent {} bytes",
            received.len()
        );
        let mut client = Client::connect(&endpoint).await.expect("still serving");
        client.request_shutdown().await.expect("stop");
        serving.await.expect("server task").expect("clean shutdown");
    });
}

#[test]
fn a_real_agent_records_before_done_and_refuses_a_missing_attachment() {
    let dir = tempfile::tempdir().expect("store");
    nanus_kernel::runtime::block_on_local(async {
        let store = JsonlStore::new(dir.path().to_owned())
            .await
            .expect("store")
            .handle();
        let runner = AgentRunner::new(
            Rc::new(Box::new(Scripted)),
            ToolRegistryHandle::new(ToolRegistry::new()),
            "you are a test",
            AgentConfig::new(4, 1, "scripted", 4096).expect("config"),
            SystemClock::new().handle(),
        )
        .expect("runner");
        let agent = Rc::new(Agent::from_parts(Parts {
            runner: Rc::new(runner),
            store: Rc::clone(&store),
            clock: SystemClock::new().handle(),
            workspace: dir.path().to_owned(),
            models: vec![String::from("scripted")],
            tools: 0,
            switch: None,
        }));
        let endpoint = endpoint(dir.path());
        let listener = bind(&endpoint).await.expect("bind");
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, agent, std::future::pending()));
        let mut client = Client::connect(&endpoint).await.expect("handshake");
        assert!(matches!(
            client.attach("missing").await,
            Err(LinkError::Agent(_))
        ));
        let attached = client.start(None).await.expect("new session");
        client
            .send(&Request::Prompt {
                text: String::from("say hello"),
            })
            .await
            .expect("prompt");
        let mut text = String::new();
        loop {
            match client.next().await.expect("frame").expect("connected") {
                Frame::Text { delta } => text.push_str(&delta),
                Frame::Done { answer, .. } => {
                    assert_eq!(answer, "hello back");
                    break;
                }
                Frame::Failed { message } => panic!("the turn failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(text, "hello back");
        let recorded = store
            .load(&SessionId::new(attached.session))
            .await
            .expect("durable");
        assert!(recorded.event_count() > 0);
        client.request_shutdown().await.expect("stop");
        serving.await.expect("server task").expect("clean shutdown");
    });
}
