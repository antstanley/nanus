//! Live cache evidence for the cache-stable request layout (C16, C17).
//!
//! These spend API credit or subscription quota, so every case is `#[ignore]`d. Run them
//! deliberately, one provider at a time:
//!
//! ```sh
//! cargo nextest run -p nanus-bundle --test live_cache --run-ignored ignored-only -E 'test(openai_subscription)'
//! ```
//!
//! Credentials are read through the same store chain a run uses and are never printed. Every body
//! is the adapter's own encoding — the managed preparation for a managed step — so this is evidence
//! about the layout nanus sends, not a hand-built imitation of it. A pass is evidence for that day
//! and model only; a promotion (`replay_across_tail`, a carrier or breakpoint flag) is a separate
//! edit with the result recorded in the change spec.
#![allow(clippy::unwrap_used, clippy::print_stderr, clippy::panic)]

use std::fmt::Write as _;

use futures::StreamExt as _;
use nanus_domain::context::managed::compile::{NoticeFacts, Selection, derive_effective_context};
use nanus_domain::context::managed::{fragments, state};
use nanus_domain::{
    Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName, ToolSchema, Usage,
};
use nanus_ports::{
    ChatRequest, LlmEvent, LlmPort, LlmStream, ManagedRequest, PromptCacheRetention,
};
use serde_json::json;

/// A system prompt long enough to clear every provider's minimum cacheable prefix.
fn long_prompt() -> String {
    let mut prompt = String::from(
        "You are a terse assistant in a caching experiment. Answer in one short sentence. ",
    );
    for index in 0..400 {
        let _ = write!(
            prompt,
            "Reference fact {index}: the experiment's fixture number {index} is unremarkable. "
        );
    }
    prompt
}

fn schema() -> ToolSchema {
    ToolSchema {
        name: ToolName::new("read").unwrap(),
        description: "Read a file by path.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
    }
}

fn work(session: &mut Session, id: &str) {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("read").unwrap(),
            json!({ "path": format!("{id}.txt") }),
        )],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new(id),
        content: format!("contents of {id}.txt: {}", "lorem ipsum ".repeat(60)),
        content_blocks: None,
        is_error: false,
    });
}

/// The managed request the runner composes for `session` now.
fn step(model: &str, session: &Session, prompt: &str) -> ChatRequest {
    let derived = fragments::derive(session.log()).unwrap();
    let protected = state::protected(session.log(), &derived);
    let events = u32::try_from(session.log().events().len()).unwrap();
    let facts = NoticeFacts {
        estimate_input_tokens: Some(events.saturating_mul(997)),
        estimator: "live".into(),
        input_allowance: 200_000,
        working_target: 200_000,
        budget_hint: false,
        recovery_available: true,
    };
    let selection = Selection {
        revision: 0,
        hidden: &[],
        notes: &[],
        notes_goal_revision: None,
    };
    let effective =
        derive_effective_context(session, &derived, &protected, selection, None, &facts).unwrap();
    let mut messages = vec![Message::system(prompt)];
    messages.extend(effective.messages);
    ChatRequest::new(model, messages)
        .with_tools(vec![schema()])
        .with_max_tokens(2_048)
        .with_context_tail(effective.tail)
        .with_prompt_cache(PromptCacheRetention::Short)
        .with_prompt_cache_key(session.id().as_str())
}

/// Three consecutive managed steps of one turn over synthetic history.
fn three_steps(model: &str, prompt: &str) -> [ChatRequest; 3] {
    let mut session = Session::new(SessionId::new("live-cache"), 0, "/live");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: "Read a.txt, then b.txt, then say done.".into(),
        content_blocks: None,
    });
    work(&mut session, "a");
    let one = step(model, &session, prompt);
    work(&mut session, "b");
    let two = step(model, &session, prompt);
    work(&mut session, "c");
    let three = step(model, &session, prompt);
    [one, two, three]
}

/// Drains a stream into its usage, failing on an error event.
async fn usage_of(stream: LlmStream) -> (Usage, Vec<LlmEvent>) {
    let events: Vec<LlmEvent> = stream.collect().await;
    for event in &events {
        if let LlmEvent::Error(message) = event {
            panic!("the provider refused the request: {message}");
        }
    }
    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no usage was reported: {events:?}"));
    (usage, events)
}

async fn managed(llm: &dyn LlmPort, request: ChatRequest) -> (Usage, Vec<LlmEvent>) {
    let call = llm
        .prepare_managed(ManagedRequest {
            request,
            selection_epoch: 0,
        })
        .unwrap_or_else(|error| panic!("{error}"));
    usage_of(call.stream()).await
}

async fn key(account: &str) -> String {
    let secrets =
        nanus_bundle::compose::open_secrets(&nanus_adapter_config::NanusConfig::default()).unwrap();
    let secret = secrets.get(account).await.unwrap().unwrap();
    assert!(!secret.is_blank(), "no credential stored for {account}");
    secret.expose().to_owned()
}

/// Asserts the later step read at least what the earlier one sent less its tail, to `granularity`.
fn assert_reads(label: &str, earlier: &Usage, later: &Usage, tail_tokens: u32, granularity: u32) {
    let floor = earlier
        .prompt_tokens
        .saturating_sub(tail_tokens)
        .checked_div(granularity)
        .unwrap_or(0)
        .saturating_mul(granularity);
    eprintln!(
        "[{label}] earlier prompt {} / later prompt {} cache hit {} (floor {floor})",
        earlier.prompt_tokens, later.prompt_tokens, later.cache_hit_tokens
    );
    assert!(
        later.cache_hit_tokens >= floor,
        "{label}: {} cached tokens, expected at least {floor}",
        later.cache_hit_tokens
    );
}

/// The tail's own cost, generously: its characters over three.
fn tail_tokens(request: &ChatRequest) -> u32 {
    let chars = request.context_tail.as_deref().map_or(0, str::len);
    u32::try_from(chars.checked_div(3).unwrap_or(0))
        .unwrap_or(u32::MAX)
        .saturating_add(16)
}

// --- C16: Anthropic --------------------------------------------------------------------------

async fn anthropic_steps(model: &str) {
    let llm = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::new(model, key("anthropic").await),
    )
    .unwrap();
    let [one, two, _] = three_steps(model, &long_prompt());
    let (first, _) = managed(&llm, one.clone()).await;
    let (second, _) = managed(&llm, two).await;
    assert_reads(model, &first, &second, tail_tokens(&one), 1);
}

/// Whether a signed reply survives the removal of the tail it was produced under, for `model`.
async fn anthropic_replay_across_tail(model: &str) {
    let api_key = key("anthropic").await;
    let llm = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::new(model, api_key.clone()),
    )
    .unwrap();
    let prompt = "Think briefly, then call the read tool on notes.txt.";
    let first = ChatRequest::new(
        model,
        vec![Message::system(long_prompt()), Message::user(prompt)],
    )
    .with_tools(vec![schema()])
    .with_max_tokens(4_096)
    .with_context_tail("[nanus context] mode=managed revision=0 step=1")
    .with_prompt_cache(PromptCacheRetention::Short);
    let (_, across) = llm.encode_with_replay_rule(&first, true);
    let (_, events) = usage_of(llm.stream_chat(first.clone())).await;
    let mut replay = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::AssistantReplay(replay) => Some(replay.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{model} returned no signed blocks: {events:?}"));
    let calls: Vec<ToolCall> = replay
        .blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| {
            ToolCall::new(
                ToolCallId::new(block["id"].as_str().unwrap()),
                ToolName::new(block["name"].as_str().unwrap()).unwrap(),
                block["input"].clone(),
            )
        })
        .collect();
    assert!(!calls.is_empty(), "{model} did not call the tool");
    replay.prefix_digest = across;
    let mut messages = first.messages.clone();
    messages.push(Message::Assistant {
        text: None,
        reasoning: Some(String::from("signed")),
        tool_calls: calls.clone(),
        replay: Some(replay),
    });
    for call in &calls {
        messages.push(Message::tool(call.id.clone(), "the notes are empty", false));
    }
    let next = ChatRequest::new(model, messages)
        .with_tools(vec![schema()])
        .with_max_tokens(4_096)
        .with_context_tail("[nanus context] mode=managed revision=0 step=2")
        .with_prompt_cache(PromptCacheRetention::Short);
    let (body, _) = llm.encode_with_replay_rule(&next, true);
    assert!(
        body.to_string().contains("\"signature\""),
        "the signed blocks are sent under the relaxed rule"
    );
    let response = reqwest::Client::new()
        .post(llm.endpoint())
        .header("x-api-key", api_key)
        .header("anthropic-version", nanus_adapter_anthropic::API_VERSION)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    eprintln!("[{model}] replay across the tail: {status}");
    assert!(
        status.is_success(),
        "{model} refused a replay with the earlier tail removed: {status} {text}"
    );
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_opus_reads_the_previous_step_from_cache() {
    anthropic_steps("claude-opus-5-5").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_sonnet_reads_the_previous_step_from_cache() {
    anthropic_steps("claude-sonnet-5-5").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_fable_reads_the_previous_step_from_cache() {
    anthropic_steps("claude-fable-5-1").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_opus_replays_across_the_tail() {
    anthropic_replay_across_tail("claude-opus-5-5").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_sonnet_replays_across_the_tail() {
    anthropic_replay_across_tail("claude-sonnet-5-5").await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn anthropic_fable_replays_across_the_tail() {
    anthropic_replay_across_tail("claude-fable-5-1").await;
}

// --- C17: OpenAI -----------------------------------------------------------------------------

const RESPONSES_MODEL: &str = "gpt-5.6-sol";
const CHAT_MODEL: &str = "gpt-5";

fn api(model: &str, api_key: String) -> nanus_adapter_openai::OpenAiLlm {
    nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        model,
        api_key,
    ))
    .unwrap()
}

/// The stored subscription grant, refused when it is about to run out.
async fn subscription_tokens() -> nanus_adapter_openai::oauth::Tokens {
    let stored = key("openai:subscription").await;
    let tokens = nanus_adapter_openai::oauth::Tokens::decode(&stored).unwrap();
    assert!(
        !tokens.is_expired(std::time::Duration::from_mins(5)),
        "the authorization has expired; run `nanus run` once to renew it"
    );
    tokens
}

async fn subscription(model: &str) -> nanus_adapter_openai::OpenAiLlm {
    subscription_with(model, subscription_tokens().await)
}

fn subscription_with(
    model: &str,
    tokens: nanus_adapter_openai::oauth::Tokens,
) -> nanus_adapter_openai::OpenAiLlm {
    let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        nanus_adapter_openai::Vendor::OpenAi,
        model,
        tokens.access_token.clone(),
        nanus_adapter_openai::OPENAI_SUBSCRIPTION_BASE_URL,
    );
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    if let Some(account) = tokens.account_id {
        config.set_account_id(account);
    }
    nanus_adapter_openai::OpenAiLlm::new(config).unwrap()
}

/// The answer text a stream produced.
fn answer(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A developer system message with no `instructions` is accepted and obeyed.
async fn obeys_the_developer_system_message(llm: &dyn LlmPort, model: &str) {
    let request = ChatRequest::new(
        model,
        vec![
            Message::system("Whatever you are asked, reply with exactly the word PINEAPPLE."),
            Message::user("Say hello."),
        ],
    )
    .with_prompt_cache(PromptCacheRetention::Short);
    let (_, events) = usage_of(llm.stream_chat(request)).await;
    let text = answer(&events);
    eprintln!("[{model}] system-directed answer: {text}");
    assert!(text.to_uppercase().contains("PINEAPPLE"), "{text}");
}

/// Three managed steps: a trailing developer tail after tool output is accepted, the system and
/// head breakpoints are accepted where they are sent, and each step reads the previous one.
async fn managed_steps_read_the_previous_step(llm: &dyn LlmPort, model: &str) {
    let [one, two, three] = three_steps(model, &long_prompt());
    let (first, _) = managed(llm, one.clone()).await;
    let (second, _) = managed(llm, two.clone()).await;
    let (third, _) = managed(llm, three).await;
    assert_reads(model, &first, &second, tail_tokens(&one), 128);
    assert_reads(model, &second, &third, tail_tokens(&two), 128);
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn openai_api_responses_accepts_the_layout_and_reads_its_cache() {
    let llm = api(RESPONSES_MODEL, key("openai").await);
    obeys_the_developer_system_message(&llm, RESPONSES_MODEL).await;
    managed_steps_read_the_previous_step(&llm, RESPONSES_MODEL).await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn openai_api_chat_accepts_a_trailing_developer_tail() {
    let llm = api(CHAT_MODEL, key("openai").await);
    managed_steps_read_the_previous_step(&llm, CHAT_MODEL).await;
}

/// Three managed steps whose requests are accepted: the trailing developer tail after tool output,
/// with no `instructions`.
async fn managed_steps_are_accepted(llm: &dyn LlmPort, model: &str) {
    for request in three_steps(model, &long_prompt()) {
        let (usage, _) = managed(llm, request).await;
        eprintln!(
            "[{model}] accepted: prompt {} cache hit {}",
            usage.prompt_tokens, usage.cache_hit_tokens
        );
    }
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn openai_subscription_accepts_the_layout() {
    let llm = subscription(RESPONSES_MODEL).await;
    obeys_the_developer_system_message(&llm, RESPONSES_MODEL).await;
    managed_steps_are_accepted(&llm, RESPONSES_MODEL).await;
}

#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn openai_subscription_reads_the_previous_step_from_cache() {
    let llm = subscription(RESPONSES_MODEL).await;
    managed_steps_read_the_previous_step(&llm, RESPONSES_MODEL).await;
}

// --- The subscription's cache routing ------------------------------------------------------------

/// The model the subscription's routing was observed on.
const ROUTING_MODEL: &str = "gpt-6.1-sol";

/// Which headers a routing probe sends beside the adapter's own body.
///
/// The probe sets every header itself rather than going through the adapter's transport, so an
/// arm stays what it names whatever the adapter sends today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    /// The session's id in `prompt_cache_key` alone: what nanus sent before 2026-11-10.
    KeyOnly,
    /// The session's id in a `session-id` header as well: what nanus sends since.
    SessionHeader,
    /// `session-id`, `thread-id` and `x-client-request-id`, and the `x-codex-turn-state` the
    /// backend answered the turn's first request with, echoed on its later ones.
    TurnState,
}

impl Arm {
    fn parse(name: &str) -> Self {
        match name.trim() {
            "key-only" => Self::KeyOnly,
            "session" => Self::SessionHeader,
            "turn-state" => Self::TurnState,
            other => panic!("no probe arm named {other:?}"),
        }
    }
}

/// One step's usage, as the backend reported it.
#[derive(Clone, Copy, Debug)]
struct Read {
    prompt: u64,
    cached: u64,
}

/// The usage in a `response.completed` event of a streamed Responses body.
fn completed_usage(text: &str) -> Option<Read> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .map(|event| {
            let usage = &event["response"]["usage"];
            Read {
                prompt: usage["input_tokens"].as_u64().unwrap_or(0),
                cached: usage["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            }
        })
}

/// One tool step of synthetic history: a call and a result of a few hundred tokens.
fn tool_step(messages: &mut Vec<Message>, id: &str) {
    let call = ToolCall::new(
        ToolCallId::new(id),
        ToolName::new("read").unwrap(),
        json!({ "path": format!("{id}.txt") }),
    );
    messages.push(Message::Assistant {
        text: None,
        reasoning: None,
        tool_calls: vec![call],
        replay: None,
    });
    let content = format!("contents of {id}.txt: {}", "lorem ipsum dolor ".repeat(150));
    messages.push(Message::tool(ToolCallId::new(id), content, false));
}

/// Runs one turn of `steps` sequential requests for `arm`, and returns each step's usage.
///
/// The body is the adapter's own ordinary encoding; only the headers differ between arms. The
/// session's id starts with a nonce and so does its system prompt, so no earlier run's entry can
/// stand in for this one's.
async fn routing_turn(
    tokens: &nanus_adapter_openai::oauth::Tokens,
    arm: Arm,
    steps: usize,
) -> Vec<Read> {
    let llm = subscription_with(ROUTING_MODEL, tokens.clone());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let session = format!("probe-{nonce}");
    let mut messages = vec![
        Message::system(format!("Run {nonce}. {}", long_prompt())),
        Message::user("Read the files one by one; reply with one word."),
    ];
    let client = reqwest::Client::new();
    let mut turn_state: Option<String> = None;
    let mut reads = Vec::new();
    for step in 0..steps {
        tool_step(&mut messages, &format!("f{step}"));
        let request = ChatRequest::new(ROUTING_MODEL, messages.clone())
            .with_tools(vec![schema()])
            .with_reasoning_effort(nanus_ports::ReasoningEffort::Low)
            .with_prompt_cache(PromptCacheRetention::Short)
            .with_prompt_cache_key(session.as_str());
        let mut sent = client
            .post(llm.endpoint_for(ROUTING_MODEL))
            .header("accept", "text/event-stream")
            .bearer_auth(&tokens.access_token)
            .json(&llm.encode(&request));
        if let Some(account) = tokens.account_id.as_deref() {
            sent = sent.header("chatgpt-account-id", account);
        }
        if arm != Arm::KeyOnly {
            sent = sent.header("session-id", session.as_str());
        }
        if arm == Arm::TurnState {
            sent = sent
                .header("thread-id", session.as_str())
                .header("x-client-request-id", session.as_str());
            if let Some(state) = turn_state.as_deref() {
                sent = sent.header("x-codex-turn-state", state);
            }
        }
        let response = sent.send().await.unwrap();
        let status = response.status();
        if turn_state.is_none() {
            turn_state = response
                .headers()
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
        }
        let text = response.text().await.unwrap();
        assert!(
            status.is_success(),
            "{arm:?} step {step}: {status} {}",
            text.replace(&tokens.access_token, "<redacted>")
        );
        reads.push(completed_usage(&text).unwrap_or_else(|| panic!("{arm:?}: no usage")));
    }
    eprintln!(
        "[{arm:?}] backend answered with a turn state: {}",
        turn_state.is_some()
    );
    reads
}

/// Prints a turn's reads: each step's cached tokens against the previous step's prompt.
fn report(arm: Arm, run: usize, reads: &[Read]) -> (usize, usize) {
    let mut line = format!("[{arm:?} run {run}]");
    let mut hits = 0_usize;
    for (step, read) in reads.iter().enumerate() {
        let previous = step.checked_sub(1).map_or(0, |at| reads[at].prompt);
        let _ = write!(
            line,
            " s{step}: {}/{} (prev {previous})",
            read.cached, read.prompt
        );
        // A step reads the previous one when it is served at least that prompt less 256 tokens.
        if step > 0 && read.cached.saturating_add(256) >= previous {
            hits = hits.saturating_add(1);
        }
    }
    eprintln!("{line}");
    (hits, reads.len().saturating_sub(1))
}

/// A bounded A/B of the subscription's cache routing.
///
/// Each arm runs `NANUS_PROBE_REPS` turns (3 by default) of `NANUS_PROBE_STEPS` sequential
/// requests (5 by default) over a
/// prompt of about eight thousand tokens, the arms interleaved so a change in the backend's load
/// reaches all of them. The arms are `NANUS_PROBE_ARMS`, comma-separated, from `key-only`,
/// `session` and `turn-state` (`key-only,session` by default). It prints every step and asserts
/// nothing about rates, because the backend is noisy and a pass would be one day's evidence.
///
/// On 2026-11-10 with `gpt-6.1-sol`, three turns of each: `key-only` read the previous step on 0
/// of 12 steps (one step read an older entry), `session` on 12 of 12.
#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn openai_subscription_cache_routing_probe() {
    let tokens = subscription_tokens().await;
    let arms: Vec<Arm> = std::env::var("NANUS_PROBE_ARMS")
        .unwrap_or_else(|_| "key-only,session".into())
        .split(',')
        .map(Arm::parse)
        .collect();
    let reps: usize = std::env::var("NANUS_PROBE_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    let steps: usize = std::env::var("NANUS_PROBE_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    let mut totals = vec![(0_usize, 0_usize); arms.len()];
    for run in 0..reps {
        for (at, arm) in arms.iter().enumerate() {
            let reads = routing_turn(&tokens, *arm, steps).await;
            let (hits, of) = report(*arm, run, &reads);
            totals[at] = (
                totals[at].0.saturating_add(hits),
                totals[at].1.saturating_add(of),
            );
        }
    }
    for (arm, (hits, of)) in arms.iter().zip(totals) {
        eprintln!("[{arm:?}] read the previous step on {hits} of {of} steps");
    }
}

// --- C17: DeepSeek ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn deepseek_reads_a_step_two_behind() {
    let model = nanus_adapter_deepseek::MODEL_FLASH;
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new(model, key("deepseek").await),
    )
    .unwrap();
    let [one, two, three] = three_steps(model, &long_prompt());
    let (first, _) = managed(&llm, one).await;
    let _ = managed(&llm, two).await;
    let (third, _) = managed(&llm, three).await;
    // Whole units only: step N+2 reads at least what step N sent.
    assert_reads(model, &first, &third, 0, 64);
}

// --- Inception ---------------------------------------------------------------------------------

/// A step that reads a file and is then handed a long listing, so each request grows by thousands
/// of tokens: a read of the system prompt alone is then plainly not a read of the conversation.
fn listing(session: &mut Session, id: &str) {
    work(session, id);
    let mut content = String::new();
    for line in 0..300_u32 {
        let _ = write!(
            content,
            "{id}.txt line {line}: value {} of the record. ",
            line.saturating_mul(7)
        );
    }
    session.append(SessionEvent::UserMessage {
        text: format!("Also note this listing:\n{content}"),
        content_blocks: None,
    });
}

/// Inception documents a cached-input price and nothing of how its cache works.
///
/// Measured on 2026-10-29 with `mercury-3`: a step reads the previous one less a few hundred to
/// about a thousand tokens, with or without the tail, and even an identical request repeated is
/// not read to its end. So the floor is a share of the previous prompt rather than the previous
/// prompt less its tail. The prompt starts with a nonce, so an earlier run's entries cannot
/// stand in for this run's.
#[tokio::test]
#[ignore = "live: spends API credit"]
async fn inception_reads_the_previous_step_from_cache() {
    let model = "mercury-3";
    let llm = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::Inception,
        model,
        key("inception").await,
    ))
    .unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prompt = format!("Run {nonce}. {}", long_prompt());
    let mut session = Session::new(SessionId::new(format!("live-{nonce}")), 0, "/live");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: "Read the files, then say done.".into(),
        content_blocks: None,
    });
    let mut earlier: Option<Usage> = None;
    for id in ["a", "b", "c", "d"] {
        listing(&mut session, id);
        let (usage, _) = managed(&llm, step(model, &session, &prompt)).await;
        if let Some(earlier) = earlier {
            let floor = earlier
                .prompt_tokens
                .saturating_mul(9)
                .checked_div(10)
                .unwrap_or(0);
            eprintln!(
                "[{model}] earlier prompt {} / later prompt {} cache hit {} (floor {floor})",
                earlier.prompt_tokens, usage.prompt_tokens, usage.cache_hit_tokens
            );
            assert!(usage.cache_hit_tokens >= floor, "{model}: below {floor}");
        }
        earlier = Some(usage);
    }
}

/// Most of a turn's later steps read the previous step's prompt through the adapter's transport.
///
/// A read falls short of the previous prompt by what follows its last eligible message and by the
/// cache's 128-token granularity — 74 to 176 tokens on 2026-11-10 — so a step counts as reading
/// the previous one within 256. With the `session-id` header a step still misses outright now and
/// then (2 of 21 measured that day, `gpt-6.1-sol`), so the case asks for two of three. Without
/// the header, three turns read the previous step on none of their twelve later steps.
#[tokio::test]
#[ignore = "live: spends subscription quota"]
async fn openai_subscription_reads_each_step_through_the_adapter() {
    let llm = subscription(ROUTING_MODEL).await;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut messages = vec![
        Message::system(format!("Run {nonce}. {}", long_prompt())),
        Message::user("Read the files one by one; reply with one word."),
    ];
    let mut reads = Vec::new();
    for step in 0..4 {
        tool_step(&mut messages, &format!("f{step}"));
        let request = ChatRequest::new(ROUTING_MODEL, messages.clone())
            .with_tools(vec![schema()])
            .with_reasoning_effort(nanus_ports::ReasoningEffort::Low)
            .with_prompt_cache(PromptCacheRetention::Short)
            .with_prompt_cache_key(format!("live-{nonce}"));
        let (usage, _) = usage_of(llm.stream_chat(request)).await;
        reads.push(Read {
            prompt: u64::from(usage.prompt_tokens),
            cached: u64::from(usage.cache_hit_tokens),
        });
    }
    let (hits, of) = report(Arm::SessionHeader, 0, &reads);
    assert!(hits >= 2, "{hits} of {of} steps read the previous step");
}

// --- Where the tail goes on DeepSeek's and Inception's chat wires ------------------------------

/// Where a placement probe puts the managed tail.
///
/// The probe places the tail itself on the adapter's tail-free encoding, rather than asking the
/// adapter to, so an arm stays what it names whatever the adapter sends today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// Appended to the final message's content, after a blank line.
    Appended,
    /// A trailing `system` message after the final user message or tool results.
    TrailingSystem,
    /// A trailing `user` message after the final user message or tool results.
    TrailingUser,
    /// No tail at all: the ceiling a placement can approach.
    Untailed,
}

impl Placement {
    fn parse(name: &str) -> Self {
        match name.trim() {
            "appended" => Self::Appended,
            "system" => Self::TrailingSystem,
            "user" => Self::TrailingUser,
            "none" => Self::Untailed,
            other => panic!("no placement named {other:?}"),
        }
    }

    fn place(self, messages: &mut serde_json::Value, tail: &str) {
        let turns = messages.as_array_mut().unwrap();
        match self {
            Self::Appended => {
                let content = &mut turns.last_mut().unwrap()["content"];
                let text = content.as_str().unwrap();
                *content = json!(format!("{text}\n\n{tail}"));
            }
            Self::TrailingSystem => turns.push(json!({ "role": "system", "content": tail })),
            Self::TrailingUser => turns.push(json!({ "role": "user", "content": tail })),
            Self::Untailed => {}
        }
    }
}

/// A chat-completions adapter a placement probe encodes with.
enum Chat {
    DeepSeek(nanus_adapter_deepseek::DeepSeekLlm),
    Inception(nanus_adapter_openai::OpenAiLlm),
}

impl Chat {
    fn encode(&self, request: &ChatRequest) -> serde_json::Value {
        match self {
            Self::DeepSeek(llm) => llm.encode(request),
            Self::Inception(llm) => llm.encode(request),
        }
    }

    fn endpoint(&self) -> String {
        match self {
            Self::DeepSeek(llm) => llm.endpoint(),
            Self::Inception(llm) => llm.endpoint(),
        }
    }
}

/// What one streamed chat completion said.
#[derive(Debug, Default)]
struct Reply {
    text: String,
    reasoning: String,
    calls: Vec<(String, String, String)>,
    prompt: u64,
    cached: u64,
}

/// Reads a chat-completions event stream: text, reasoning, calls by index, and the usage.
fn read_reply(stream: &str) -> Reply {
    let mut reply = Reply::default();
    let chunks = stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok());
    for chunk in chunks {
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            reply.prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
            reply.cached = [
                &usage["prompt_cache_hit_tokens"],
                &usage["prompt_tokens_details"]["cached_tokens"],
                &usage["cached_input_tokens"],
            ]
            .iter()
            .filter_map(|value| value.as_u64())
            .max()
            .unwrap_or(0);
        }
        let delta = &chunk["choices"][0]["delta"];
        reply.text.push_str(delta["content"].as_str().unwrap_or(""));
        reply
            .reasoning
            .push_str(delta["reasoning_content"].as_str().unwrap_or(""));
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = usize::try_from(call["index"].as_u64().unwrap_or(0)).unwrap();
            if reply.calls.len() <= index {
                reply
                    .calls
                    .resize(index.saturating_add(1), Default::default());
            }
            let slot = &mut reply.calls[index];
            slot.0.push_str(call["id"].as_str().unwrap_or(""));
            slot.1
                .push_str(call["function"]["name"].as_str().unwrap_or(""));
            slot.2
                .push_str(call["function"]["arguments"].as_str().unwrap_or(""));
        }
    }
    reply
}

/// A file of a few thousand tokens whose first line names its magic number.
fn large_file(path: &str, magic: u32) -> String {
    let mut content = format!("{path}\nmagic: {magic}\n");
    for line in 0..300_u32 {
        let _ = writeln!(
            content,
            "{path} row {line}: reading {} of the survey.",
            line.saturating_mul(13).saturating_add(magic)
        );
    }
    content
}

/// Records a reply's calls and answers each with a large file.
fn answer_calls(session: &mut Session, reply: &Reply) -> Vec<String> {
    let calls: Vec<ToolCall> = reply
        .calls
        .iter()
        .map(|(id, name, arguments)| {
            let arguments = serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments));
            ToolCall::new(ToolCallId::new(id), ToolName::new(name).unwrap(), arguments)
        })
        .collect();
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some(reply.text.clone()).filter(|text| !text.is_empty()),
        reasoning: Some(reply.reasoning.clone()).filter(|text| !text.is_empty()),
        tool_calls: calls.clone(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    let mut paths = Vec::new();
    for call in calls {
        let path = call.arguments["path"].as_str().unwrap_or("?").to_owned();
        let magic = match path.as_str() {
            "f0.txt" => 11,
            "f1.txt" => 23,
            "f2.txt" => 37,
            _ => 0,
        };
        session.append(SessionEvent::ToolResult {
            call_id: call.id.clone(),
            content: large_file(&path, magic),
            content_blocks: None,
            is_error: false,
        });
        paths.push(path);
    }
    paths
}

/// One turn under `placement`: the model reads three large files one call at a time and sums
/// their magic numbers (71). Returns each step's usage up to a refusal, which it prints.
async fn placement_turn(
    chat: &Chat,
    api_key: &str,
    model: &str,
    placement: Placement,
) -> Vec<Read> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prompt = format!("Run {nonce}. {}", long_prompt());
    let mut session = Session::new(SessionId::new(format!("live-{nonce}")), 0, "/live");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: "Read f0.txt, then f1.txt, then f2.txt, with one read call per step. Each file's \
               second line is `magic: N`. Once all three are read, reply with only the sum of \
               the three magic numbers."
            .into(),
        content_blocks: None,
    });
    let client = reqwest::Client::new();
    let mut reads = Vec::new();
    let mut line = format!("[{placement:?}]");
    for at in 0..5 {
        let mut request = step(model, &session, &prompt).with_max_tokens(4_096);
        let tail = request.context_tail.take().unwrap();
        let mut body = chat.encode(&request);
        placement.place(&mut body["messages"], &tail);
        let response = client
            .post(chat.endpoint())
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap()
            .replace(api_key, "<redacted>");
        if !status.is_success() {
            eprintln!("{line} s{at}: REFUSED {status} {text}");
            return reads;
        }
        let reply = read_reply(&text);
        reads.push(Read {
            prompt: reply.prompt,
            cached: reply.cached,
        });
        if reply.calls.is_empty() {
            let _ = write!(line, " s{at}: answer {:?}", reply.text.trim());
            break;
        }
        let paths = answer_calls(&mut session, &reply);
        let _ = write!(line, " s{at}: read {paths:?}");
    }
    eprintln!("{line}");
    reads
}

/// A bounded A/B of where the tail goes, over the adapter's own tail-free encoding.
///
/// Each placement runs `NANUS_PROBE_REPS` turns (3 by default), interleaved, from
/// `NANUS_TAIL_ARMS` (`appended,system,user,none` by default). Each turn is up to five steps over
/// a prompt of about seven thousand tokens, each tool result about 4,500 more. It prints every
/// step's cached tokens against the previous step's prompt and asserts nothing about rates.
///
/// On 2026-11-10, three turns of each, every request was accepted and every turn read the three
/// files in order; one `user` turn on `deepseek-flash` summed them wrongly. Mean tokens a later
/// step read short of the previous step's prompt, over nine steps per arm:
///
/// | Arm | `deepseek-flash` | `mercury-3` |
/// |---|---|---|
/// | `appended` (the wire) | 265 | 341 |
/// | `system` | 288 | 274 |
/// | `user` | 277 | 331 |
/// | `none` | 12 | 275 |
///
/// `DeepSeek` reads in 128-token units, so every tailed arm loses the last few units however the
/// tail is carried. Inception's per-step spread (0 to 750 tokens, untailed included) is wider
/// than any difference between arms. So no placement was better, and the wire was not changed.
async fn placement_probe(chat: Chat, api_key: &str, model: &str) {
    let arms: Vec<Placement> = std::env::var("NANUS_TAIL_ARMS")
        .unwrap_or_else(|_| "appended,system,user,none".into())
        .split(',')
        .map(Placement::parse)
        .collect();
    let reps: usize = std::env::var("NANUS_PROBE_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    for run in 0..reps {
        for placement in &arms {
            let reads = placement_turn(&chat, api_key, model, *placement).await;
            let mut line = format!("[{model} {placement:?} run {run}]");
            for (at, read) in reads.iter().enumerate() {
                let previous = at.checked_sub(1).map_or(0, |earlier| reads[earlier].prompt);
                let short = previous.saturating_sub(read.cached);
                let _ = write!(
                    line,
                    " s{at}: {}/{} (prev {previous}, short {short})",
                    read.cached, read.prompt
                );
            }
            eprintln!("{line}");
        }
    }
}

/// The two placement adapters, with a credential nothing reads.
fn offline_chats() -> [(Chat, &'static str); 2] {
    let deepseek = nanus_adapter_deepseek::MODEL_FLASH;
    [
        (
            Chat::DeepSeek(
                nanus_adapter_deepseek::DeepSeekLlm::new(
                    nanus_adapter_deepseek::DeepSeekConfig::new(deepseek, "unused"),
                )
                .unwrap(),
            ),
            deepseek,
        ),
        (
            Chat::Inception(
                nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
                    nanus_adapter_openai::Vendor::Inception,
                    "mercury-3",
                    "unused",
                ))
                .unwrap(),
            ),
            "mercury-3",
        ),
    ]
}

/// The probe's `appended` arm is byte for byte what each adapter sends, after a plain user message
/// and after a tool result, so the probe measures the wire. A trailing arm is not: it adds one
/// message and leaves every message the adapter encoded as it was.
#[test]
fn the_placement_probe_s_appended_arm_is_the_adapters_own_wire() {
    let mut session = Session::new(SessionId::new("offline"), 0, "/live");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: "Read a.txt.".into(),
        content_blocks: None,
    });
    let mut worked = session.clone();
    work(&mut worked, "a");
    for (chat, model) in offline_chats() {
        for history in [&session, &worked] {
            let request = step(model, history, "prompt");
            let mut bare = request.clone();
            let tail = bare.context_tail.take().unwrap();
            let canonical = chat.encode(&bare);
            let mut appended = canonical.clone();
            Placement::Appended.place(&mut appended["messages"], &tail);
            assert_eq!(appended, chat.encode(&request), "{model}");
            let mut trailing = canonical.clone();
            Placement::TrailingSystem.place(&mut trailing["messages"], &tail);
            assert_ne!(trailing, chat.encode(&request), "{model}");
            let (last, before) = trailing["messages"]
                .as_array()
                .unwrap()
                .split_last()
                .unwrap();
            assert_eq!(before, canonical["messages"].as_array().unwrap().as_slice());
            assert_eq!(last["content"], json!(tail));
        }
    }
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn deepseek_tail_placement_probe() {
    let model = nanus_adapter_deepseek::MODEL_FLASH;
    let api_key = key("deepseek").await;
    let llm = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new(model, api_key.clone()),
    )
    .unwrap();
    placement_probe(Chat::DeepSeek(llm), &api_key, model).await;
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn inception_tail_placement_probe() {
    let model = "mercury-3";
    let api_key = key("inception").await;
    let llm = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::Inception,
        model,
        api_key.clone(),
    ))
    .unwrap();
    placement_probe(Chat::Inception(llm), &api_key, model).await;
}
