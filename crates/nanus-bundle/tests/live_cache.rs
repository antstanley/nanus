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

async fn subscription(model: &str) -> nanus_adapter_openai::OpenAiLlm {
    let stored = key("openai:subscription").await;
    let tokens = nanus_adapter_openai::oauth::Tokens::decode(&stored).unwrap();
    assert!(
        !tokens.is_expired(std::time::Duration::from_mins(1)),
        "the authorization has expired; run `nanus run` once to renew it"
    );
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
