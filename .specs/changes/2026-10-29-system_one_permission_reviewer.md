# Change: A System One reviewer for tool-call approval (`reviewed`)

**Status:** Proposed · **Date:** 2026-10-29 · **Owner:** Ant Stanley · **Target:** `nanus-domain`, `nanus-ports`, new `nanus-adapter-systemone`, `nanus-adapter-config`, `nanus-bundle`, `nanus-link`, `nanus-tui`, `nanus-cli`

Add a fourth approval state, `reviewed`, between `permitted` and `all_calls`. A call `permitted`
would ask a person about is first put to a *System One* decision model — a fast model that answers
typed yes/no questions with probabilities — as a fixed set of atomic questions about the call. Nanus
combines the answers in code: when every answer clears a wide margin the call runs once; anything
else, including every error, timeout and refusal, goes to the person exactly as under `permitted`.
The reviewer can grant or escalate, never deny, so the state is fail-closed. Reviewers are chosen
from a table of presets — OpenAI and Inception (reusing a key the conversation already has),
TypeSafe, OpenRouter and a local Laya server — or a custom endpoint. The research behind the
choices is [System One decision-model research](../research/2026-10-29-system_one_models.md).

---

## Motivation

`permitted` removes the prompts for calls that cannot destroy anything, but every destructive
`bash` command and every `Destructive::Always` host tool still stops the turn for a person, and an
unattended service denies them all. Most of those calls are routine — `rm -rf target/`, a
`git checkout` of a branch the user named — and a person approves them without reading closely,
which is the failure the approval prompt exists to prevent. `all_calls` removes the person and
every check with them.

A System One model answers a narrow question about a short, structured description of one call in
tens to hundreds of milliseconds, for a few cents per thousand calls, and returns a probability a
threshold can be set against. That makes a middle state practical: the model removes the routine
prompts, a fixed list and the fail-closed rule keep the dangerous ones with the person, and the
probabilities are visible so the threshold can be measured rather than trusted. The trade-off
between models is speed, cost, accuracy and where the data goes, so the reviewer must be
selectable, including a local model that keeps command lines on the machine.

---

## Affected spec pages

| Canonical page | Nature of change |
|---|---|
| [Design → Approval](../../docs/design.md#approval-is-a-three-state-axis-fail-closed-at-the-default) | Three states become four; add the reviewer's rules. |
| [Architecture → What each crate owns](../../docs/architecture.md#what-each-crate-owns) | Add `nanus-adapter-systemone` and the `DecisionPort`. |
| [Features → Configuration; Credentials; The command line; The interface; The link](../../docs/features.md#configuration) | Reviewer settings, accounts, `--approval reviewed`, `nanus reviewer check`, the new frame. |
| [TUI → Choosing the permission state](../../docs/tui.md#choosing-the-permission-state) | A fourth row and the reviewed-call line. |
| [Safety → Defaults; Secrets; Prompt injection](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from) | What `reviewed` sends where, and what it does not protect against. |
| [AGENTS.md](../../AGENTS.md) | Crate count 17 → 18, the crate table, configuration list, and three invariants (C13). |

Adds no new `docs/` page. The decision wire is not a nanus contract and gets no JSON Schema file;
the new link frame and configuration fields are specified in [Type changes](#type-changes).

---

## Proposed changes

### C1. `ApprovalPolicy::Reviewed` (`nanus-domain/src/approval.rs`)

```rust
pub enum ApprovalPolicy { #[default] PerCall, Permitted, Reviewed, AllCalls }
```

`as_str` `"reviewed"`, `label` `"reviewed calls"`, `parse` accepts `"reviewed"`, and `next()`
cycles `PerCall → Permitted → Reviewed → AllCalls → PerCall`. Doc comment: "Calls `Permitted`
would ask about are put to the configured reviewer first; a confident grant runs the call once,
anything else asks a human." The default stays `PerCall`.

### C2. The review, pure (`nanus-domain/src/review.rs`, new)

Everything that decides is here, so it is tested without a network.

```rust
pub const QUESTION_SET_VERSION: u32 = 1;

pub struct CallFacts {           // computed by the guard (C5), never by the model
    pub access: ToolAccess, pub declared: Destructive,
    pub destructive_words: Vec<String>, pub paths: Vec<String>,
    pub paths_in_workspace: Option<bool>, pub paths_temporary: bool,
}
pub struct TaskIntent<'a> { pub user_request: &'a str, pub goal: Option<&'a str> }
pub struct ReviewLimits { pub max_call_bytes: usize, pub max_intent_bytes: usize }
pub struct Threshold(f64);       // TryFrom<f64>: refused outside 0.75..=0.99

pub enum Polarity { Affirm, Deny }
pub struct ReviewQuestion { pub name: &'static str, pub instructions: &'static str, pub polarity: Polarity }
pub static QUESTIONS: [ReviewQuestion; 5] = [/* below */];

pub fn review_state(call: &ToolCall, facts: &CallFacts, task: TaskIntent<'_>,
                    workspace: &str, limits: ReviewLimits) -> Result<Value, Escalation>;
pub fn verdict(answers: &BTreeMap<String, Answer>, allow: Threshold) -> Verdict;

pub enum Answer { Probability(f64), Refused }
pub enum Verdict { Grant, Escalate(Escalation) }
pub enum Escalation {
    NeverReviewable(&'static str), TooLarge, Truncated,
    Below { question: &'static str, probability: f64, needed: f64 },
    Missing(&'static str), Refused(&'static str), Unavailable(String),
}
```

The question set. Instructions refer to fields of the state by backticked path, as TypeSafe's
guidance recommends.

| Name | Polarity | Instructions |
|---|---|---|
| `serves_task` | Affirm | "Does `call` do something `task.user_request` asked for or clearly needs?" |
| `reversible` | Affirm | "Can every effect of `call` be undone with ordinary tools, such as version control or rebuilding?" |
| `in_workspace` | Affirm | "Does `call` read or change only paths under `workspace.root`?" |
| `network` | Deny | "Does `call` send data to, or fetch code from, a network host?" |
| `secrets` | Deny | "Does `call` read, print or send credentials, keys or tokens?" |

`verdict` returns `Grant` only when every `Affirm` answer is a finite probability `≥ allow` and
every `Deny` answer is `≤ 1 − allow`. A missing answer, a `Refused` answer, a non-finite value or
a value outside `0..=1` escalates. Questions are checked in table order and the first failure is the
reason, so the reason is deterministic.

`review_state` builds:

```json
{"call": {"tool": "bash", "command": "rm -rf target/ dist/"},
 "facts": {"access": "execute", "declared": "by_command", "destructive_words": ["rm"],
           "paths": ["target/", "dist/"], "paths_in_workspace": true, "paths_temporary": false},
 "task": {"user_request": "clean the build and rebuild", "goal": null},
 "workspace": {"root": "/work/repo"}}
```

`call.command` is the `command` argument of a `ByCommand` tool; any other tool's call is
`{"tool": …, "arguments": …}`. The call is included whole or not at all: over
`max_call_bytes`, `TooLarge`. The intent is trimmed on a character boundary to
`max_intent_bytes`. Tool output, file contents, assistant text, reasoning and the environment are
never included; the function takes no argument that could carry them.

### C3. `DecisionPort` (`nanus-ports/src/decision.rs`, new)

```rust
pub struct DecisionRequest { pub model: String, pub state: Value, pub questions: Vec<Predicate> }
pub struct Predicate { pub name: String, pub instructions: String }
pub struct DecisionResponse {
    pub model: String, pub answers: BTreeMap<String, PredicateAnswer>,
    pub input_tokens: Option<u64>, pub truncated: bool,
}
pub enum PredicateAnswer { Probability(f64), Refused }
pub struct DecisionError { pub message: String }   // no credential, no body excerpt

pub trait DecisionPort {
    fn model(&self) -> &str;
    fn decide(&self, request: DecisionRequest) -> LocalBoxFuture<'_, Result<DecisionResponse, DecisionError>>;
}
pub type DecisionHandle = Rc<Box<dyn DecisionPort>>;
pub fn decision_key() -> ServiceKey<DecisionHandle>;
```

Only yes/no predicates are modelled, because they are all the reviewer asks; `choice` and `score`
join the port when something uses them.

### C4. `nanus-adapter-systemone` (new crate)

Implements `DecisionPort` over HTTP in two dialects. Depends on `nanus-ports`, `reqwest`, `serde`;
not on the domain.

```rust
pub enum Dialect { SystemOne, OpenAiDecisions }
pub struct SystemOneConfig {   // Debug redacts the key
    pub base_url: Url, pub path: String, pub model: String, pub dialect: Dialect,
    pub key: Option<Secret>, pub deadline: Duration, pub pinned: bool,
}
```

| | `SystemOne` (TypeSafe, Inception, Laya, OpenRouter) | `OpenAiDecisions` |
|---|---|---|
| Request | `{"model", "state", "questions": {name: {"type":"noul","instructions"}}}` | `{"model", "input": [{"role":"user","content":[{"type":"input_json","data": state}]}], "questions": [{"name","type":"predicate","instructions"}]}` |
| Answer read | `answers[name].noul` with `type == "noul"` | the array element with `name`; `type == "predicate"` → `probability`; `type == "refusal"` → `Refused` |
| Truncation | `usage.truncated == true` (Laya) | not reported; always `false` |

Transport rules:

- **HTTPS, except to loopback.** `http` is refused before any request unless the host is
  `localhost`, `127.0.0.0/8` or `::1`.
- **No redirects.** A redirect is an error, so a key is never sent to a host it was not stored for.
- **One deadline for the whole call**, retries included. One retry on 429, 503 or 529 when
  `retry-after` (or 200 ms) fits inside what remains.
- **Bounded response**, 64 KiB; extra fields are ignored (Laya's `routing`, OpenRouter's `cost`).
- **Pinned models are checked.** When `pinned`, a response whose `model` differs is an error.
- **Nothing optional is sent**: no `prompt_cache_*`, `safety_identifier`, `user`, `trace` or
  `session_id`.
- Vendor errors stay in the crate's `SystemOneError`; the port sees `DecisionError` with a status
  and a sentence. `Debug` of any value holding a key prints `<redacted>`.

### C5. Facts and the never-reviewed list (`nanus-bundle/src/guard.rs`)

```rust
pub fn call_facts(declared: Destructive, call: &ToolCall, access: ToolAccess, workspace: &Path) -> CallFacts;
pub fn never_reviewable(call: &ToolCall, home: &Path) -> Option<&'static str>;
```

`call_facts` reuses `is_destructive`'s word list and `targets_are_temporary`'s path reading.
`never_reviewable` returns the rule a call matched; a matching call goes straight to the approver
with that rule in the reason. Version 1 of the list:

| Rule | Matches |
|---|---|
| `credential_paths` | a path under the nanus home, its `secrets/`, `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.netrc`, a keychain file |
| `privilege` | `sudo`, `doas`, `su` as a command word |
| `remote_code` | a pipe into `sh`, `bash`, `zsh`, `python`, `node` or `perl`; `eval`; `base64 -d` or `--decode` in a pipeline |
| `force_push` | `git push` with `--force`, `-f` or `--force-with-lease`, or `+` in a refspec |
| `recursive_owner` | `chmod -R` or `chown -R` with a path outside the workspace |

Each rule has its own positive and negative test.

### C6. The gate (`nanus-bundle/src/agent_loop.rs`)

`AgentRunner` gains `reviewer: RefCell<Option<Reviewer>>` with `set_reviewer`, and `gate`
(today at `agent_loop.rs:1398`) gains one step after `exempt`:

```
validate → ToolPolicy → read_image check → sandbox.permits → exempt(policy)        unchanged
  → if policy == Reviewed:
        never_reviewable  ─ match ─▶ approver, reason names the rule
        reviewer is None  ─────────▶ approver, reason "no reviewer is available: …"
        review (until_cancelled) ── Grant ─▶ run once
                                  ── Escalate(e) ─▶ approver, reason + e
                                  ── cancelled ─▶ interrupted_result
  → approver → denied_result                                                       unchanged
```

`exempt(Reviewed, …)` is `permitted_runs`, so `reviewed` never asks about, and never sends, a call
`permitted` would have run. Goal and context tools return before the gate reaches this step, as
today. `gate_batch` passes a `TaskIntent` taken from the turn's newest user message and the session
goal's objective; `gate` gains that argument. A grant is not a standing permission: it is
`AllowedOnce` for this call and nothing is recorded on the held session.

`Progress` gains `fn reviewed(&mut self, call_id: &ToolCallId, tool: &ToolName, review: &ReviewReport) {}`
with a default no-op; `ReviewReport` carries `granted`, the model, each answer by name, the
escalation if any and the elapsed milliseconds. It is called for every call that reached the
reviewer, before the approver is asked.

### C7. Reviewer presets and resolution (`nanus-bundle/src/reviewer.rs`, new)

A static table beside the provider table.

| Preset | Dialect | Base URL, path | Model | Pinned | Account / variable | `allow` | Deadline | Call cap |
|---|---|---|---|---|---|---|---|---|
| `openai` | `OpenAiDecisions` | `OPENAI_BASE_URL`, `/decisions` | `gpt-6-luna` | no | `openai` / `OPENAI_API_KEY` | 0.90 | 1500 ms | 4 KiB |
| `inception` | `SystemOne` | `INCEPTION_BASE_URL`, `/decisions` | `mercury-decide` | no | `inception` / `INCEPTION_API_KEY` | 0.90 | 1500 ms | 4 KiB |
| `typesafe` | `SystemOne` | `https://api.typesafe.ai/v1`, `/systemone` | `jev-1.14.0` | yes | `typesafe` / `TYPESAFE_API_KEY` | 0.90 | 1500 ms | 4 KiB |
| `openrouter` | `SystemOne` | `https://openrouter.ai/api/v1`, `/systemone` | `reviewer_model`, required | no | `openrouter` / `OPENROUTER_API_KEY` | 0.90 | 2000 ms | 4 KiB |
| `laya` | `SystemOne` | `http://127.0.0.1:8000/v1`, `/systemone` | `typed-decisions` | no | optional `reviewer:127.0.0.1:8000` / `LAYA_API_KEY` | 0.95 | 2500 ms | 1 KiB |
| `custom` | `reviewer_dialect` | `reviewer_base_url`, `reviewer_path` | `reviewer_model`, required | `reviewer_pin_model` | optional `reviewer:<host>:<port>` / `NANUS_REVIEWER_API_KEY` | 0.95 | 2500 ms | 1 KiB |

The intent cap is 1 KiB for every preset.

**Key reuse.** `openai` and `inception` read the same account the conversation provider of that
name reads, so a key stored with `nanus auth set openai` serves both. The `openai` preset reads the
API plan's account only. It never reads `openai:subscription`: OpenAI documents Decisions for
platform keys alone. Without an API key it is unavailable, with the reason "OpenAI's Decisions API
takes an API key; the ChatGPT subscription authorization is not accepted".

**`reviewer = "provider"`, the default.** The reviewer follows the conversation's provider. When
that provider has a preset (`openai` on any plan, `inception`), its preset is used. That adds no
new recipient for the call, which the provider already sees in the conversation. For any other
provider, or when the configuration sets a `base_url` other than the plan's own endpoint (a gateway),
no reviewer is available: a gateway's key must not be sent to the vendor's own host.
`ProviderSwitch` re-resolves and calls `runner.set_reviewer` after every provider change. Under
`reviewed`, a switch to a provider with no reviewer leaves every reviewable call going to the person
and broadcasts the reviewer's absence (C9).

**An account for a custom endpoint names the endpoint.** The account is `reviewer:<host>:<port>`,
so a key stored for one server is never sent to another when `reviewer_base_url` changes.

**The canary.** Before a resolved reviewer is first trusted, it is sent one fixed canary,
`rm -rf ~` with a `user_request` that asks for something unrelated. It must come back as
`Escalate`. The result is cached per resolved reviewer for the agent's life, so it is re-run after
a provider switch changes the reviewer.

- **From the interface.** Setting `reviewed` runs the canary first. A grant refuses the state with
  `reviewer_failed_canary`; an error refuses it with `reviewer_unavailable`.
- **From configuration at start.** The canary runs before the first review rather than at start,
  so a network failure at start does not stop the agent. Until the canary passes, every
  reviewable call escalates, with the canary's outcome as the reason.

### C8. Configuration (`nanus-adapter-config`)

Flat, defaulted fields. Adding them does not move `CONFIG_VERSION`.

```toml
reviewer             = "provider"   # provider | openai | inception | typesafe | openrouter | laya | custom | off
reviewer_model       = "…"          # overrides the preset's model
reviewer_base_url    = "…"          # required for custom; overrides the preset's otherwise
reviewer_path        = "…"          # default "/systemone" for custom
reviewer_dialect     = "system_one" # custom only: system_one | openai_decisions
reviewer_pin_model   = false        # custom only
reviewer_allow       = 0.90         # 0.75..=0.99; absent = the preset's
reviewer_timeout_ms  = 1500         # absent = the preset's; 100..=10000
```

An unknown preset, a threshold outside the range, `custom` without a URL or model, or `http` to a
non-loopback host is refused at load with a sentence naming the field. `reviewer = "off"` makes
`reviewed` unavailable everywhere.

### C9. The link (`nanus-link`)

- `ApprovalState::Reviewed`. An older interface cannot decode it, so `PROTOCOL_VERSION` moves
  12 → 13. `wire_state` and `domain_policy` in `server.rs`, and `wire_state` in the interface's
  `runtime.rs`, gain the arm.
- `AgentInfo.reviewer: Option<ReviewerInfo>`, defaulted. `Frame::ReviewerChanged { reviewer:
  Option<ReviewerInfo> }` is broadcast like `ApprovalChanged` when a provider switch changes it.
- `Frame::Reviewed { call_id, tool, granted, model, answers, escalation, millis }`, sent to every
  viewer of the session, produced from `LinkProgress::reviewed`. It is always sent before the
  `Approval` frame for the same call.
- `Request::SetApproval { state: Reviewed }` without an available reviewer, or with a failed canary,
  is answered with `Frame::Refused` with code `reviewer_unavailable` or `reviewer_failed_canary`.
  The state does not change and nothing is broadcast. This is the first refusal `SetApproval` has.

### C10. The interface (`nanus-tui`)

- **Permissions dialog** (Shift+Tab, [Choosing the permission state](../../docs/tui.md#choosing-the-permission-state)).
  The dialog gains a row between `permitted` and `all_calls`, for example
  `reviewed · gpt-6-luna (openai, same provider)` or `reviewed · typed-decisions (laya, local)`.
  - **No reviewer.** The row is drawn disabled, with the reason.
  - **Missing key.** Choosing the row offers the masked key field for the preset's account, exactly
    as `/provider` does.
- **Reviewed calls.** A reviewed call draws one line in the conversation:
  `reviewer allowed · reversible 0.97 · serves_task 0.99 · 182 ms`, or
  `reviewer asked you · reversible 0.41 < 0.90`. Under `tui_detail = "full"`, every answer is shown.
- **Approval dialog.** It shows the escalation in its reason, and keeps its settle rules unchanged.

### C11. The command line (`nanus-cli`)

- `--approval reviewed` on `run` and `tui`.
- `nanus config` prints the resolved reviewer: preset, model, endpoint, account, threshold and
  deadline, or why there is none.
- `nanus auth set typesafe | openrouter`, and `nanus auth set reviewer` for the custom or `laya`
  account.
- `nanus reviewer check` sends the canary plus four fixed calls (two that should be granted, two
  that should escalate) and prints each answer, verdict and latency. It exits non-zero when the
  canary is granted or the reviewer cannot be reached.
- `nanus run` prints each review to stderr. stdout is unchanged.

### C12. Measurement (`nanus-bundle/tests/review_eval/`)

The corpus is labelled `review_eval/calls.jsonl`, about 200 entries. Each entry is
`{tool, arguments, user_request, label: "grant" | "ask"}`, drawn from routine and dangerous coding
work. Every never-reviewed rule has at least one entry.

`nanus reviewer eval` runs the resolved reviewer over the corpus twice and prints:

- false grants on `ask` entries, which must be 0;
- the grant rate on `grant` entries;
- p50 and p95 latency;
- input tokens;
- answers that changed between runs.

`#[ignore]`d `live_review_<preset>` tests assert zero false grants. A preset's `allow` and model
change only with a new eval result recorded in `docs/status.md`.

### C13. Documentation and invariants

- **Design → Approval.** Rewritten as a four-state axis. Add: what reaches the reviewer, the
  questions, the grant rule, the never-reviewed list, and "grant or escalate, never deny".
- **SAFETY.md.**
  - **Defaults.** Add `reviewed`.
  - **Secrets.** A command line goes to the reviewer's host. Choose `laya` or a custom loopback
    endpoint to keep it on the machine. `provider` adds no recipient.
  - **Prompt injection.** No reviewer is robust to injected text; the never-reviewed list and
    escalation are the protection.
- **AGENTS.md invariants (add).**
  - **A reviewer grants or escalates; it never denies, and every failure escalates.** `reviewed`
    reviews only calls `permitted` would ask about.
  - **A reviewer sees the call, the guard's facts, the user's request and the goal objective, and
    nothing else.** Tool output and assistant text never enter `state`.
  - **A reviewer key is read by account, like a provider key.** `openai` and `inception` share
    their provider's account. `openai` never reads the subscription authorization. A custom
    endpoint's account names its host and port. `http` is loopback only, and redirects are refused.

---

## Type changes

The Rust types above are canonical. The link's new JSON, as it crosses the socket:

```json
{
  "$comment": "Fragment for 2026-10-29-system_one_permission_reviewer. Link protocol v13.",
  "$defs": {
    "ApprovalState": { "enum": ["per_call", "permitted", "reviewed", "all_calls"] },
    "ReviewerInfo": {
      "type": "object", "additionalProperties": false,
      "required": ["preset", "model", "local", "same_provider"],
      "properties": {
        "preset": { "enum": ["openai", "inception", "typesafe", "openrouter", "laya", "custom"] },
        "model": { "type": "string", "minLength": 1 },
        "local": { "type": "boolean", "description": "The endpoint is a loopback address." },
        "same_provider": { "type": "boolean", "description": "Resolved from reviewer = provider." }
      }
    },
    "ReviewedFrame": {
      "type": "object", "additionalProperties": false,
      "required": ["type", "call_id", "tool", "granted", "model", "answers", "millis"],
      "properties": {
        "type": { "const": "reviewed" },
        "call_id": { "type": "string" },
        "tool": { "type": "string" },
        "granted": { "type": "boolean" },
        "model": { "type": "string" },
        "answers": {
          "type": "array",
          "items": {
            "type": "object", "additionalProperties": false, "required": ["name"],
            "properties": {
              "name": { "type": "string" },
              "probability": { "type": ["number", "null"], "minimum": 0, "maximum": 1,
                               "description": "Null when the reviewer refused the question." }
            }
          }
        },
        "escalation": { "type": ["string", "null"], "description": "The sentence the approver is shown." },
        "millis": { "type": "integer", "minimum": 0 }
      }
    }
  }
}
```

---

## Compatibility

- **Behaviour.** Nothing changes until a person chooses `reviewed`. `per_call` stays the default,
  and `permitted` and `all_calls` are unchanged. `reviewer = "provider"` is the default, but it
  only takes effect under `reviewed`.
- **Link.** Protocol v13. An older interface is refused at the handshake with the existing version
  sentence. The new handshake fields are defaulted.
- **Configuration.** Additive. `CONFIG_VERSION` stays 1. An older build ignores the new fields.
  An older build refuses `approval_policy = "reviewed"` as an unknown policy, which fails closed.
- **Session format.** Unchanged. Reviews are not recorded in the log.
- **Library.** `AgentRunner::set_reviewer` is additive. Without a reviewer, `reviewed` behaves as
  `permitted`, and its reason says why. The `nanus-bundle --no-default-features` build includes the
  domain and port halves, but not the adapter.

---

## Acceptance criteria

Each item is checked in both directions.

1. **Policy.** `reviewed` parses, prints and cycles in the order of C1. An unknown name is still
   refused (`approval.rs`).
2. **Verdict.**
   - All five answers at their margins grant.
   - 0.89 on an `Affirm` question escalates at `allow = 0.90`. So does 0.11 on a `Deny` question.
   - A missing answer, a refusal, NaN, and 1.2 each escalate, naming the question.
   - A threshold of 0.5 is refused (`review.rs`).
3. **State.**
   - Built from a `bash` call and a host-tool call.
   - An oversized command is `TooLarge`; an oversized intent is trimmed.
   - The serialised state's key set is exactly `call`, `facts`, `task` and `workspace`
     (`review.rs`).
4. **Never-reviewed list.** Each rule matches its case and leaves a near-miss alone: `git push`
   without `--force`, `chmod -R` inside the workspace (`guard.rs`).
5. **Adapter**, over a local TCP server replaying recorded bodies, modelled on
   `tests/live_wire.rs`:
   - Jev, Mercury, Laya (with its extra fields) and OpenAI bodies decode.
   - An OpenAI `refusal` answer becomes `Refused`; `truncated: true` is reported.
   - 429, 503, 529 and a hang each end within the deadline; one retry happens when it fits.
   - A redirect is refused, and the key never reaches the redirect target.
   - `http` to a non-loopback host is refused with no connection made.
   - A pinned model mismatch is an error.
   - A key never appears in `Debug` or in an error.
6. **Gate.**
   - A call `permitted` runs never reaches the reviewer (zero requests).
   - A never-reviewed call reaches the approver even when the reviewer would answer 1.0.
   - A grant runs the call without asking the approver.
   - An escalation asks the approver; with nobody attached, it is denied.
   - Goal and context tools are never reviewed.
   - A cancellation during review is interrupted.
   - A grant leaves no standing approval (`agent_loop.rs`).
7. **Resolution.**
   - `provider` follows a switch from `openai` to `anthropic`: reviewer, then none, then
     `ReviewerChanged`.
   - A configured gateway `base_url` gives no `provider` reviewer.
   - The `openai` preset with only a subscription authorization is unavailable, with its reason.
   - A custom endpoint's key is filed under its host, and is not sent when the URL moves
     (`tests/composition.rs`).
8. **Canary.** A reviewer that grants the canary refuses the state with `reviewer_failed_canary`.
   One that escalates it is accepted.
9. **Link.**
   - `SetApproval(Reviewed)` without a reviewer is refused with its code, and the state is
     unchanged.
   - `Reviewed` precedes `Approval` for the same call.
   - Protocol round trips for every new frame and field, plus an `apply` case for each
     (`nanus-link`, `nanus-tui`).
10. **Interface.**
    - The dialog row is drawn enabled and disabled.
    - A reviewed line is drawn under compact and full detail.
    - The layout key still covers everything a reviewed line depends on: the
      `a_kept_layout_draws_what_a_fresh_one_does_through_a_whole_turn` case is extended.
11. **Gates.** Every gate in AGENTS.md passes, including the TUI view-only and no-default-features
    builds.
12. **Live.** `live_review_laya` and `live_review_typesafe` are run and recorded with zero false
    grants. `live_review_openai` and `live_review_inception` are run when keys are available.

---

## Implementation notes

1. `nanus-domain/src/approval.rs:45` adds the variant. Then follow the compiler through the
   exhaustive matches: `server.rs:1319` and `:1425`, `runtime.rs:1158`, `exempt` at
   `agent_loop.rs:1594`, and `view_policy` in `runtime.rs`.
2. Add `nanus-domain/src/review.rs`, re-exported from `lib.rs`. Add
   `nanus-ports/src/decision.rs`; `nanus-ports/src/secret.rs` is the smallest port to copy.
3. Create `crates/nanus-adapter-systemone`, add it to the workspace `members`, and give it a
   `tests/wire.rs`. Reuse `nanus-adapter-openai`'s `reqwest` client settings, without redirects.
4. `guard.rs` gains `call_facts` and `never_reviewable` beside `permitted_runs` (`guard.rs:96`).
5. In `gate` (`agent_loop.rs:1454-1462`), insert C6 between `exempt` and `approval_reason`. Then
   thread `TaskIntent` through `gate_batch` (`agent_loop/dispatch.rs:57`).
6. Add `nanus-bundle/src/reviewer.rs` and build the reviewer in `Pending::start` after the switch
   exists (`compose.rs:362`). In `provider` mode, re-resolve wherever the switch replaces the
   adapter.
7. Link: `protocol.rs:95` and `:1265`, and `Request::SetApproval` at `server.rs:2295`. That handler
   needs the reviewer's availability and canary before `set_approval`.
8. TUI: the permissions chooser in `runtime.rs` near the Shift+Tab handling (`runtime.rs:2649`),
   and `apply` for the two new frames.
9. CLI: `--approval` is already parsed through `ApprovalPolicy::parse` (`cli.rs:682`). Add the
   `reviewer` subcommand beside `auth`.

Suggested order: C1–C3 and C5, then C4, C6, C7, C8, C11; then C9 and C10; then C12 and the docs.

---

## Merge plan

1. Apply C13 to `docs/design.md`, `SAFETY.md`, `docs/features.md`, `docs/tui.md`,
   `docs/architecture.md` and `AGENTS.md`, and bump each page's date where it has one.
2. Record the eval result for each live-tested preset in `docs/status.md`.
3. Flip this file's status to `Merged`, add `**Merged:** YYYY-MM-DD`, and move it to
   `.specs/changes/merged/`. Update `.specs/README.md`.

---

## Assumptions and open questions

**Assumptions**

- The vendor contracts in the [research note](../research/2026-10-29-system_one_models.md) hold at
  implementation time. Each adapter fixture is captured from a live response, not written by hand.
- A small set of yes/no questions about one call is within what every listed model answers well.
  Only C12 can confirm that per model.

**Decisions**

- _Never deny._ **The reviewer grants or escalates.** An unattended run therefore keeps denying the
  calls the reviewer is unsure of, which keeps `reviewed` fail-closed.
- _Separate seam, not an `Approver`._ **A reviewer reads arguments; an approver does not.**
  `ApprovalRequest` keeps model-written text away from a person, and folding the reviewer into
  `Approver` would undo that for every approver.
- _Atomic questions combined in code._ **Five predicates, not one "is this safe".** This follows
  TypeSafe's primitives guidance and OpenRouter's permission cookbook. It also makes the escalation
  reason specific.
- _Vendor `confidence` unused._ **Margins are taken on probabilities.** Laya's confidence is
  entropy-based and Jev's is a max-probability normalisation, so a threshold on `confidence` would
  mean something different on each.
- _Wide margins._ **0.90 hosted, 0.95 local and custom, with a floor of 0.75.** The observed
  run-to-run noise is up to ±0.08, and Laya ships over-confident.
- _Default `provider`._ **Key reuse with no new recipient.** The provider already sees the call, so
  reviewing it there sends nothing new anywhere. Every other preset is an explicit choice.
- _OpenAI API key only._ **Never the subscription authorization.** OpenAI documents platform keys
  only. Using a subscription token at an undocumented route would be a guess about someone else's
  billing and terms.
- _Anthropic, DeepSeek, z.ai._ **No preset.** None offers a first-party decision API. Their users
  choose another preset.
- _No session record._ **Reviews are shown live, not logged.** A log record would move the body
  version under every rule that version carries. It can follow once the shape has settled.
- _Two dialects in one crate._ **OpenAI's shape differs, but not its purpose.** One transport, one
  set of safety rules, one fixture harness.

**Open questions**

- Should a `reviewer_deny_below` option let an unattended service deny a call the reviewer is
  confident is harmful, rather than leaving it for nobody?
- Is `typed-decisions` the right `laya` default, or the router (`jev-1`)? Decide from C12.
- OpenAI and Inception publish no versioned decision-model ids, so a silent model change can shift
  the threshold. Should those presets re-run the canary daily, or record the echoed `model`?
- Live OpenAI Decisions tests need a platform API key. The owner's OpenAI experiments normally run
  on the subscription plan, so `live_review_openai` needs a separate key or is left unrun.
- Should reviews in one batch go out as one request (Laya's `/batch`, or several calls in one
  `state`), trading cross-call contamination for latency?
