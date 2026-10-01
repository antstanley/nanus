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
