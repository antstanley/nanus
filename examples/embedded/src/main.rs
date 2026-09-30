//! A caller-owned local executor, model and clock, with save-before-acknowledgment.
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{AgentConfig, Session, SessionId, ToolRegistry};
use nanus_ports::{ChatRequest, ClockPort, LlmEvent, LlmPort, LlmStream};
use std::{error::Error, rc::Rc};

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        0
    }
}
struct Model;
impl LlmPort for Model {
    fn model(&self) -> &str {
        "caller-owned"
    }
    fn stream_chat(&self, _: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::iter([LlmEvent::TextDelta(
            "Hello from the host".into(),
        )]))
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let runner = AgentRunner::new(
        Rc::new(Box::new(Model)),
        ToolRegistryHandle::new(ToolRegistry::new()),
        "Caller-owned trusted prompt template",
        AgentConfig::new(4, 1, "caller-owned", 4096)?,
        Rc::new(Box::new(Clock)),
    )?;
    let mut session = Session::new(SessionId::new("embedding-example"), 0, "caller workspace");
    let result = runtime.block_on(runner.run_turn(&mut session, "Hello", &mut Silent, None))?;
    // The host owns persistence. A real desktop host uses its StorePort; this tiny host
    // writes its bounded payload to a new file and syncs it before acknowledging success.
    let saved = session.try_to_jsonl()?;
    let destination =
        std::env::temp_dir().join(format!("nanus-embedded-{}.jsonl", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::Write::write_all(&mut file, saved.as_bytes())?;
    file.sync_all()?;
    println!("{}", result.answer);
    Ok(())
}
